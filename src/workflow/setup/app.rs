//! The setup wizard's ratatui application (ADR 0020): one state machine
//! over [`Screen`]s, driven by crossterm key events, painting every frame
//! through the shared TUI substrate. This replaces the inquire-driven
//! screen functions whose opt-in `clear_frame` calls kept leaving stacked
//! frames in the terminal — whole-frame diffed repaint makes that class of
//! defect structurally impossible: there is no code path that can print a
//! line without repainting the screen it belongs to.
//!
//! The flow graph is byte-for-byte the old one (ADR 0011 mode-first entry,
//! ADR 0019 provider steps + soft model gate + session model cache, R3
//! auto-verify with Edit / Retry / Save-anyway), so the domain behavior and
//! its tests carry over unchanged; only the surface moved. Slow operations
//! (model fetch, probes, the preset smoke check) run on background threads
//! and land on the event loop through a channel, so the spinner keeps
//! animating and the UI never blocks.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::cli_flow::{CliRow, cli_menu_rows};
use super::finalize;
use super::provider::{
    ProviderEntry, api_key_label, applicable_steps, base_url_label, cache_key, model_label,
    picker_rows, preview_model,
};
use super::verify::smoke_check;
use super::*;
use crate::core::config::{Source, resolve_base_url, resolve_field};
use crate::llm::cli_agent::cli_preset;
use crate::render::layout::MARGIN;
use crate::render::tui::picker::{self, Pick, Picker, PickerItem};
use crate::render::tui::style as tstyle;
use crate::render::tui::terminal::Tui;
use crate::render::tui::text::{TextAct, TextInput};

/// One spinner step per poll tick — 100 ms per braille frame.
const TICK: Duration = Duration::from_millis(100);

/// The braille spinner frames (ADR 0018 marker vocabulary).
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How the event loop left the wizard.
#[derive(Debug)]
enum Flow {
    Running,
    Save(Config),
    Cancel,
}

/// The background job being waited on, and what its result means.
struct Pending {
    kind: PendingKind,
    rx: mpsc::Receiver<JobDone>,
}

enum PendingKind {
    /// Fetching a provider's live model list; `key` is the session-cache
    /// key the result is stored under (ADR 0019 §5).
    Models { key: String },
    /// A backend verify probe (R3) against `target`.
    Probe { target: String },
    /// The `--version` smoke check of a CLI preset, chained into a probe.
    Smoke,
}

enum JobDone {
    Models(Vec<String>),
    Probe(Result<String>),
    Smoke(String),
}

/// Which backend a verify outcome belongs to — routes the ✓/✗ marker onto
/// the right top-menu row and the failure screen's `Edit` back into the
/// right sub-flow.
#[derive(Clone, Copy, PartialEq)]
enum ProbeFlow {
    Provider,
    Cli,
}

impl ProbeFlow {
    /// The top-menu row to highlight when this flow settles.
    fn highlight(self) -> usize {
        match self {
            ProbeFlow::Provider => 1,
            ProbeFlow::Cli => 2,
        }
    }
}

/// The Verify result block (R3): everything the outcome and failure screens
/// render, captured once when the probe lands.
struct VerifyBlock {
    flow: ProbeFlow,
    target: String,
    /// The model's trimmed reply on success.
    reply: Option<String>,
    /// The propagated error on failure.
    error: Option<String>,
    causes: Vec<String>,
    /// Provider-flow context rows (masked key / effective base URL).
    context: Vec<Line<'static>>,
}

/// One screen of the wizard. Every variant repaints from scratch through
/// [`App::draw`]; none carries bytes of a previous frame.
#[derive(Clone)]
enum Screen {
    /// First-run Backend choice (ADR 0011 mode-first).
    ModeChoice(Picker),
    TopMenu(Picker),
    BackendChoice(Picker),
    ProviderPick(Picker),
    ProviderMenu(Picker),
    /// The optional-key menu (OpenAI-compatible providers).
    ApiKeyMenu(Picker),
    /// `from_menu`: Esc returns to [`Screen::ApiKeyMenu`] instead of the
    /// provider sub-menu (the text prompt is a sub-mode of the menu).
    ApiKeyText(TextInput, bool),
    BaseUrlMenu(Picker),
    BaseUrlText(TextInput, bool),
    ModelPick(Picker),
    /// `custom`: entered from the picker's `Custom model…` row, so Esc goes
    /// back to the picker; otherwise this is the required free-text
    /// fallback (fetch failed, ADR 0019 §4) and Esc goes to the sub-menu.
    ModelText(TextInput, bool),
    CliPick(Picker),
    /// A background job is running; the frame is a spinner + label. `back`
    /// is the screen Esc restores — aborting detaches the job (its result
    /// is dropped when the channel's receiver goes away), so a wedged
    /// network call can never trap the user on this screen.
    Busy {
        label: String,
        back: Box<Screen>,
    },
    /// A successful probe, waiting for Enter.
    VerifyOutcome,
    /// A failed probe: the error block plus Edit / Retry / Save anyway (R3).
    FailExit(Picker),
    /// Esc on a dirty top menu: confirm the discard (default yes).
    DiscardConfirm,
}

/// The wizard state machine. `draft`, `existing`, and the flow graph it
/// walks are the same model the inquire wizard drove; only the surface
/// changed.
pub(super) struct App {
    deps: Arc<SetupDeps>,
    existing: Option<Config>,
    existing_provider: Option<Provider>,
    draft: Draft,
    screen: Screen,
    /// Top-menu row to re-highlight after a sub-flow settles.
    highlight: usize,
    pending: Option<Pending>,
    /// The smoke-check one-liner riding along with the CLI verify screens.
    smoke_line: Option<String>,
    verify: Option<VerifyBlock>,
    ticks: u64,
}

/// Run the wizard to a save or a cancel. Blocking event loop; the caller
/// owns the [`Tui`] terminal guard so teardown (and the post-exit notices)
/// happen in `run_setup` on every path.
pub(super) fn run(
    deps: Arc<SetupDeps>,
    existing: Option<Config>,
    tui: &mut Tui,
) -> Result<Option<Config>> {
    let existing_provider = existing
        .as_ref()
        .and_then(|c| c.backend.as_deref())
        .map(Provider::from_name);
    let draft = finalize::seed_draft(&existing);
    let mut app = App::new(deps, existing, existing_provider, draft);
    loop {
        tui.draw(|f| app.draw(f))?;
        if let Some(done) = app.poll_job() {
            if let Flow::Running = app.on_job(done)? {
                continue;
            }
            unreachable!("on_job always continues the loop")
        }
        if !event::poll(TICK)? {
            app.ticks += 1;
            continue;
        }
        if let Event::Key(key) = event::read()?
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            match app.on_key(key)? {
                Flow::Running => {}
                Flow::Save(config) => return Ok(Some(config)),
                Flow::Cancel => return Ok(None),
            }
        }
    }
}

impl App {
    fn new(
        deps: Arc<SetupDeps>,
        existing: Option<Config>,
        existing_provider: Option<Provider>,
        draft: Draft,
    ) -> Self {
        let screen = if existing.is_none() {
            Screen::ModeChoice(mode_choice_picker())
        } else {
            Screen::TopMenu(top_menu_picker(&draft, &existing, 0))
        };
        Self {
            deps,
            existing,
            existing_provider,
            draft,
            screen,
            highlight: 0,
            pending: None,
            smoke_line: None,
            verify: None,
            ticks: 0,
        }
    }

    // -- flow helpers ----------------------------------------------------

    fn top_menu(&mut self) {
        self.screen = Screen::TopMenu(top_menu_picker(&self.draft, &self.existing, self.highlight));
    }

    fn provider_menu(&mut self) {
        self.screen = Screen::ProviderMenu(provider_menu_picker(&self.draft));
    }

    fn enter_model_step(&mut self) {
        let p = self.draft.provider.unwrap_or_default();
        let (api_key, _) =
            resolve_field(self.draft.api_key.as_deref().filter(|k| !k.is_empty()), "");
        let (base_url, _) =
            resolve_base_url(self.draft.base_url.as_deref().filter(|u| !u.is_empty()), &p);
        let key = cache_key(p, base_url.as_deref(), &api_key);
        if let Some(models) = self.draft.model_cache.get(&key).cloned() {
            self.show_models(models);
        } else if p.requires_key() && api_key.is_empty() {
            // A key-requiring provider with no key can only 401 — never
            // touch the network (ADR 0019 §4); cache the empty degrade.
            self.draft.model_cache.insert(key, Vec::new());
            self.show_models(Vec::new());
        } else {
            self.spawn(JobKind::Fetch {
                provider: p,
                api_key,
                base_url,
            });
            self.busy(format!("Fetching {} models…", p.display()));
        }
    }

    /// Show a fetched (or cached) model list: the picker over the capped
    /// rows, or the required free-text fallback when it came back empty
    /// (ADR 0019 §4).
    fn show_models(&mut self, models: Vec<String>) {
        if models.is_empty() {
            let initial = finalize::field_initial(
                self.draft.model.as_deref(),
                &self.existing,
                self.existing_provider,
                self.draft.provider,
                |c| c.model.as_ref(),
            );
            self.screen = Screen::ModelText(
                TextInput::new(
                    "Model (required)",
                    initial.as_deref(),
                    false,
                    "model cannot be empty",
                ),
                false,
            );
        } else {
            let initial = self
                .draft
                .model
                .as_deref()
                .filter(|m| !m.is_empty())
                .and_then(|v| models.iter().position(|m| m == v))
                .unwrap_or(0);
            self.screen = Screen::ModelPick(Picker::new_at(
                "Model",
                picker_rows(&models)
                    .into_iter()
                    .map(|m| picker::item(vec![(&m, tstyle::plain())]))
                    .chain(std::iter::once(picker::item(vec![(
                        "Custom model…",
                        tstyle::plain(),
                    )])))
                    .collect(),
                initial,
            ));
        }
    }

    /// Switch to the Busy screen, remembering the screen Esc restores. A
    /// Busy spawned from a Busy (the smoke→probe chain) keeps the original
    /// back screen — the intermediate spinner is not a place to return to.
    fn busy(&mut self, label: String) {
        let back = match &self.screen {
            Screen::Busy { back, .. } => back.clone(),
            s => Box::new(s.clone()),
        };
        self.screen = Screen::Busy { label, back };
    }

    /// Start a provider-flow probe (R3) with the draft's effective values.
    fn probe_provider(&mut self) {
        let p = self.draft.provider.unwrap_or_default();
        let target = p.display().to_lowercase();
        let model = self.draft.effective_model();
        let (api_key, _) =
            resolve_field(self.draft.api_key.as_deref().filter(|k| !k.is_empty()), "");
        let masked = api_key_label(&api_key);
        let (url, source) =
            resolve_base_url(self.draft.base_url.as_deref().filter(|u| !u.is_empty()), &p);
        let url_row = base_url_label(url.as_deref(), source);
        self.verify = Some(VerifyBlock {
            flow: ProbeFlow::Provider,
            target: target.clone(),
            reply: None,
            error: None,
            causes: vec!["Common causes: wrong API key, model name, base URL, or network.".into()],
            context: vec![
                Line::from(Span::styled(
                    format!("{MARGIN}  API key: {masked}"),
                    tstyle::muted(),
                )),
                Line::from(Span::styled(
                    format!("{MARGIN}  Base URL: {url_row}"),
                    tstyle::muted(),
                )),
            ],
        });
        self.pending_kind_probe(target, JobKind::ProbeProvider);
        self.busy(format!("Contacting {} ({model})…", p.display()));
    }

    /// Start a CLI-flow probe (R3); `smoke_first` chains the `--version`
    /// check in front of it.
    fn probe_cli(&mut self, smoke_first: Option<String>) {
        let target = self.draft.active_cli_command().unwrap_or("cli").to_string();
        self.verify = Some(VerifyBlock {
            flow: ProbeFlow::Cli,
            target: target.clone(),
            reply: None,
            error: None,
            causes: vec![
                "Common causes: the CLI is not installed, not".into(),
                "authenticated, or timed out. Run the CLI once to log in.".into(),
            ],
            context: Vec::new(),
        });
        match smoke_first {
            Some(program) => {
                self.spawn(JobKind::Smoke {
                    program: program.clone(),
                });
                self.busy(format!("Checking `{program}`…"));
            }
            None => {
                self.pending_kind_probe(target.clone(), JobKind::ProbeCli);
                self.busy(format!("Probing {target}…"));
            }
        }
    }

    fn pending_kind_probe(&mut self, target: String, kind: JobKind) {
        let pending = self.spawn_job(kind);
        self.pending = Some(Pending {
            kind: PendingKind::Probe { target },
            rx: pending,
        });
    }

    fn spawn(&mut self, kind: JobKind) {
        let rx = self.spawn_job(kind.clone());
        self.pending = Some(Pending {
            kind: match kind {
                JobKind::Fetch { .. } => PendingKind::Models {
                    key: cache_sentinel(&self.draft),
                },
                JobKind::Smoke { .. } => PendingKind::Smoke,
                JobKind::ProbeProvider | JobKind::ProbeCli => {
                    unreachable!("probes go through pending_kind_probe")
                }
            },
            rx,
        });
    }

    /// Launch `kind` on a background thread (the probes' nested runtimes
    /// cannot run on the async-runtime thread the loop lives on) and return
    /// the channel its result lands on.
    fn spawn_job(&self, kind: JobKind) -> mpsc::Receiver<JobDone> {
        let (tx, rx) = mpsc::channel();
        let deps = Arc::clone(&self.deps);
        let draft = self.draft.clone();
        thread::spawn(move || {
            let done = match kind {
                JobKind::Fetch {
                    provider,
                    api_key,
                    base_url,
                } => JobDone::Models((deps.fetch_models)(provider, &api_key, base_url.as_deref())),
                JobKind::ProbeProvider => JobDone::Probe((deps.verify_provider)(&draft)),
                JobKind::ProbeCli => JobDone::Probe((deps.verify_cli)(&draft)),
                JobKind::Smoke { program } => JobDone::Smoke(smoke_check(&program)),
            };
            let _ = tx.send(done);
        });
        rx
    }

    /// Non-blocking check for a finished job. Deliberately does NOT clear
    /// `pending`: [`App::on_job`] consumes the pending slot to learn which
    /// kind the result belongs to — clearing here would orphan the result
    /// and strand the wizard on the Busy screen (the every-fetch-hangs
    /// regression: the thread finished, the result was dropped, the spinner
    /// never stopped).
    fn poll_job(&mut self) -> Option<JobDone> {
        let rx = &self.pending.as_ref()?.rx;
        match rx.try_recv() {
            Ok(done) => Some(done),
            Err(mpsc::TryRecvError::Empty) => None,
            // The job thread died without sending; treat as a failed probe /
            // empty listing so the wizard never hangs on Busy.
            Err(mpsc::TryRecvError::Disconnected) => Some(match self.pending_kind_snapshot() {
                PendingSnapshot::Probe => JobDone::Probe(Err(anyhow::anyhow!(
                    "the background probe task did not report a result"
                ))),
                _ => JobDone::Models(Vec::new()),
            }),
        }
    }

    fn pending_kind_snapshot(&self) -> PendingSnapshot {
        match self.pending.as_ref().map(|p| &p.kind) {
            Some(PendingKind::Probe { .. }) => PendingSnapshot::Probe,
            _ => PendingSnapshot::Models,
        }
    }

    fn on_job(&mut self, done: JobDone) -> Result<Flow> {
        let kind = match self.pending.take() {
            Some(p) => p.kind,
            None => return Ok(Flow::Running),
        };
        match (kind, done) {
            (PendingKind::Models { key }, JobDone::Models(models)) => {
                let models = models.clone();
                self.draft.model_cache.insert(key, models.clone());
                self.show_models(models);
            }
            (PendingKind::Smoke, JobDone::Smoke(line)) => {
                self.smoke_line = Some(line);
                let target = self
                    .verify
                    .as_ref()
                    .map(|v| v.target.clone())
                    .unwrap_or_default();
                self.pending_kind_probe(target.clone(), JobKind::ProbeCli);
                self.busy(format!("Probing {target}…"));
            }
            (PendingKind::Probe { target }, JobDone::Probe(result)) => {
                let Some(block) = self.verify.as_mut() else {
                    self.top_menu();
                    return Ok(Flow::Running);
                };
                match result {
                    Ok(reply) => {
                        block.reply = Some(reply);
                        self.draft.verify_status = Some(VerifyStatus::backend_pass(
                            block.flow.to_backend_kind(),
                            target,
                        ));
                        self.highlight = block.flow.highlight();
                        self.screen = Screen::VerifyOutcome;
                    }
                    Err(e) => {
                        block.error = Some(format!("{e:#}"));
                        self.draft.verify_status = Some(VerifyStatus::backend_fail(
                            block.flow.to_backend_kind(),
                            target,
                        ));
                        self.screen = Screen::FailExit(fail_exit_picker());
                    }
                }
            }
            // A smoke/probe result where the other was expected cannot
            // happen (each job chains deterministically); fall back to the
            // menu rather than wedge on Busy.
            _ => self.top_menu(),
        }
        Ok(Flow::Running)
    }

    // -- input -----------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) -> Result<Flow> {
        if is_cancel(key) {
            // Ctrl-C cancels from every screen, Busy included — the guard
            // used to spare Busy, which turned a wedged network call into
            // an unkillable spinner (the job threads are read-only probes,
            // so tearing the session down mid-flight is safe).
            return Ok(Flow::Cancel);
        }
        match std::mem::replace(&mut self.screen, Screen::DiscardConfirm) {
            Screen::ModeChoice(mut p) => match p.on_key(key) {
                Some(Pick::Selected(0)) => {
                    self.draft.backend_kind = Some(BackendKind::Api);
                    self.screen = Screen::ProviderPick(provider_pick_picker(
                        &self.draft,
                        self.existing_provider,
                    ))
                }
                Some(Pick::Selected(_)) => {
                    self.draft.backend_kind = Some(BackendKind::Cli);
                    self.screen = Screen::CliPick(cli_pick_picker());
                }
                Some(Pick::Back) => self.top_menu(),
                _ => self.screen = Screen::ModeChoice(p),
            },
            Screen::TopMenu(mut p) => match p.on_key(key) {
                Some(Pick::Selected(0)) => {
                    self.screen = Screen::BackendChoice(backend_choice_picker())
                }
                Some(Pick::Selected(1)) => {
                    self.screen = Screen::ProviderPick(provider_pick_picker(
                        &self.draft,
                        self.existing_provider,
                    ))
                }
                Some(Pick::Selected(2)) => self.screen = Screen::CliPick(cli_pick_picker()),
                Some(Pick::Selected(3)) => {
                    self.draft.confirm_before_commit =
                        Some(!self.draft.confirm_before_commit.unwrap_or(false));
                    self.highlight = 3;
                    self.top_menu();
                }
                Some(Pick::Selected(4)) => {
                    self.draft.review_batch_plan =
                        Some(!self.draft.review_batch_plan.unwrap_or(false));
                    self.highlight = 4;
                    self.top_menu();
                }
                Some(Pick::Selected(_)) => {
                    return Ok(Flow::Save(finalize::finalize(clone_draft(&self.draft))));
                }
                Some(Pick::Back) => {
                    if finalize::draft_dirty(&self.draft, &self.existing) {
                        self.screen = Screen::DiscardConfirm;
                    } else {
                        return Ok(Flow::Cancel);
                    }
                }
                _ => self.screen = Screen::TopMenu(p),
            },
            Screen::BackendChoice(mut p) => match p.on_key(key) {
                Some(Pick::Selected(0)) => {
                    self.draft.backend_kind = Some(BackendKind::Api);
                    self.highlight = 0;
                    self.top_menu();
                }
                Some(Pick::Selected(_)) => {
                    self.draft.backend_kind = Some(BackendKind::Cli);
                    self.highlight = 0;
                    self.top_menu();
                }
                Some(Pick::Back) => self.top_menu(),
                _ => self.screen = Screen::BackendChoice(p),
            },
            Screen::ProviderPick(mut p) => match p.on_key(key) {
                Some(Pick::Selected(i)) => {
                    let chosen = Provider::all()[i];
                    finalize::switch_provider(&mut self.draft, chosen);
                    self.draft.provider = Some(chosen);
                    self.provider_menu();
                }
                Some(Pick::Back) => {
                    self.highlight = 1;
                    self.top_menu();
                }
                _ => self.screen = Screen::ProviderPick(p),
            },
            Screen::ProviderMenu(mut p) => {
                let entries = provider_menu_entries(&self.draft);
                match p.on_key(key) {
                    Some(Pick::Selected(i)) => match entries.get(i).copied() {
                        Some(ProviderEntry::ApiKey) => self.enter_api_key(),
                        Some(ProviderEntry::BaseUrl) => self.enter_base_url(),
                        Some(ProviderEntry::Model) => self.enter_model_step(),
                        // Soft required gate (ADR 0019 §7): Done routes into
                        // the Model step until a model exists.
                        Some(ProviderEntry::Done) if self.draft.effective_model().is_empty() => {
                            self.enter_model_step()
                        }
                        Some(ProviderEntry::Done) => self.probe_provider(),
                        None => self.provider_menu(),
                    },
                    Some(Pick::Back) => {
                        self.screen = Screen::ProviderPick(provider_pick_picker(
                            &self.draft,
                            self.existing_provider,
                        ))
                    }
                    _ => self.screen = Screen::ProviderMenu(p),
                }
            }
            Screen::ApiKeyMenu(mut p) => {
                let has_key = !effective_api_key(&self.draft).is_empty();
                let (keep_idx, no_key_idx, enter_idx) = if has_key {
                    (Some(0), 2, 1)
                } else {
                    (None, 0, 1)
                };
                match p.on_key(key) {
                    Some(Pick::Selected(i)) if keep_idx == Some(i) => self.provider_menu(),
                    Some(Pick::Selected(i)) if i == no_key_idx => {
                        self.draft.api_key = None;
                        self.provider_menu();
                    }
                    Some(Pick::Selected(i)) if i == enter_idx => {
                        self.screen = Screen::ApiKeyText(
                            TextInput::new("API key (blank = keyless server)", None, true, ""),
                            true,
                        );
                    }
                    _ => self.screen = Screen::ApiKeyMenu(p),
                }
            }
            Screen::ApiKeyText(mut t, from_menu) => match t.on_key(key) {
                Some(TextAct::Submit(v)) => {
                    let keep = effective_api_key(&self.draft);
                    self.draft.api_key = Some(if v.is_empty() { keep } else { v });
                    self.provider_menu();
                }
                Some(TextAct::Back) => {
                    if from_menu {
                        self.screen = Screen::ApiKeyMenu(api_key_menu_picker(&self.draft));
                    } else {
                        self.provider_menu();
                    }
                }
                _ => self.screen = Screen::ApiKeyText(t, from_menu),
            },
            Screen::BaseUrlMenu(mut p) => self.on_base_url_menu(&mut p, key),
            Screen::BaseUrlText(mut t, from_menu) => match t.on_key(key) {
                Some(TextAct::Submit(v)) => {
                    self.draft.base_url = Some(v);
                    self.provider_menu();
                }
                Some(TextAct::Back) => {
                    if from_menu {
                        self.screen = Screen::BaseUrlMenu(base_url_menu_picker(&self.draft));
                    } else {
                        self.provider_menu();
                    }
                }
                _ => self.screen = Screen::BaseUrlText(t, from_menu),
            },
            Screen::ModelPick(mut p) => match p.on_key(key) {
                Some(Pick::Selected(i)) if is_custom_row(&self.draft, i) => {
                    self.screen = Screen::ModelText(
                        TextInput::new("Custom model", None, false, "model cannot be empty"),
                        true,
                    );
                }
                Some(Pick::Selected(i)) => {
                    self.draft.model = Some(model_at(&self.draft, i));
                    self.provider_menu();
                }
                Some(Pick::Back) => self.provider_menu(),
                _ => self.screen = Screen::ModelPick(p),
            },
            Screen::ModelText(mut t, custom) => match t.on_key(key) {
                Some(TextAct::Submit(v)) => {
                    self.draft.model = Some(v);
                    self.provider_menu();
                }
                Some(TextAct::Back) => {
                    if custom {
                        self.enter_model_step();
                    } else {
                        self.provider_menu();
                    }
                }
                _ => self.screen = Screen::ModelText(t, custom),
            },
            Screen::CliPick(mut p) => {
                let rows = cli_menu_rows();
                match p.on_key(key) {
                    Some(Pick::Selected(i)) => match rows.get(i).copied() {
                        Some(CliRow::Preset(name)) => {
                            let spec = cli_preset(name).expect("PRESETS rows have specs");
                            let program = spec.command.to_string();
                            self.draft.cli = CliConfig {
                                command: Some(spec.command),
                                args: Some(spec.args),
                                timeout_secs: Some(spec.timeout_secs),
                                encoding: Some(spec.encoding),
                            };
                            self.probe_cli(Some(program));
                        }
                        Some(CliRow::Done) => {
                            if self.draft.active_cli_command().is_some() {
                                self.probe_cli(None);
                            } else {
                                self.highlight = 2;
                                self.top_menu();
                            }
                        }
                        None => self.screen = Screen::CliPick(p),
                    },
                    Some(Pick::Back) => {
                        self.highlight = 2;
                        self.top_menu();
                    }
                    _ => self.screen = Screen::CliPick(p),
                }
            }
            Screen::Busy { label, back } => {
                if key.code == KeyCode::Esc {
                    // Abort: detach the job (dropping the receiver makes its
                    // result a no-op when the thread finishes) and return to
                    // the screen that started it.
                    self.pending = None;
                    self.screen = *back;
                } else {
                    self.screen = Screen::Busy { label, back };
                }
            }
            Screen::VerifyOutcome => match key.code {
                KeyCode::Enter | KeyCode::Esc => self.top_menu(),
                _ => self.screen = Screen::VerifyOutcome,
            },
            Screen::FailExit(mut p) => match p.on_key(key) {
                Some(Pick::Selected(0)) => match self.verify.as_ref().map(|v| v.flow) {
                    Some(ProbeFlow::Cli) => self.screen = Screen::CliPick(cli_pick_picker()),
                    _ => self.provider_menu(),
                },
                Some(Pick::Selected(1)) => {
                    let flow = self.verify.as_ref().map(|v| v.flow);
                    let target = self
                        .verify
                        .as_ref()
                        .map(|v| v.target.clone())
                        .unwrap_or_default();
                    match flow {
                        Some(ProbeFlow::Cli) => {
                            self.pending_kind_probe(target, JobKind::ProbeCli);
                            self.busy("Probing…".into());
                        }
                        _ => self.probe_provider(),
                    }
                }
                Some(Pick::Selected(_)) => {
                    self.highlight = self
                        .verify
                        .as_ref()
                        .map(|v| v.flow.highlight())
                        .unwrap_or(0);
                    self.top_menu();
                }
                Some(Pick::Back) => {
                    self.highlight = self
                        .verify
                        .as_ref()
                        .map(|v| v.flow.highlight())
                        .unwrap_or(0);
                    self.top_menu();
                }
                _ => self.screen = Screen::FailExit(p),
            },
            Screen::DiscardConfirm => match key.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    return Ok(Flow::Cancel);
                }
                KeyCode::Char('n') | KeyCode::Char('N') => self.top_menu(),
                KeyCode::Esc => self.top_menu(),
                _ => self.screen = Screen::DiscardConfirm,
            },
        }
        Ok(Flow::Running)
    }

    fn on_base_url_menu(&mut self, p: &mut Picker, key: KeyEvent) {
        let provider = self
            .draft
            .provider
            .expect("provider chosen before base url");
        let (_, source) = resolve_base_url(
            self.draft.base_url.as_deref().filter(|u| !u.is_empty()),
            &provider,
        );
        let has_url = matches!(source, Source::Config);
        let (keep_idx, use_default_idx, custom_idx) = if has_url {
            (Some(0), 1, 2)
        } else {
            (None, 0, 1)
        };
        match p.on_key(key) {
            Some(Pick::Selected(i)) if keep_idx == Some(i) => self.provider_menu(),
            Some(Pick::Selected(i)) if i == use_default_idx => {
                self.draft.base_url = None;
                self.provider_menu();
            }
            Some(Pick::Selected(i)) if i == custom_idx => {
                self.screen = Screen::BaseUrlText(
                    TextInput::new("Custom base URL", None, false, "base URL cannot be empty"),
                    true,
                );
            }
            Some(Pick::Back) => self.provider_menu(),
            _ => {
                self.screen = Screen::BaseUrlMenu(std::mem::replace(p, Picker::new("", Vec::new())))
            }
        }
    }

    fn enter_api_key(&mut self) {
        let provider = self.draft.provider.expect("provider chosen before api key");
        let key = effective_api_key(&self.draft);
        if provider.requires_key() {
            let prompt = if key.is_empty() {
                "API key".to_string()
            } else {
                format!(
                    "API key (current: {} — leave blank to keep)",
                    finalize::mask_key(&key)
                )
            };
            let allow_empty = !key.is_empty();
            self.screen = Screen::ApiKeyText(
                TextInput::new(&prompt, None, allow_empty, "API key cannot be empty"),
                false,
            );
        } else if provider == Provider::OpenAiCompatible {
            self.screen = Screen::ApiKeyMenu(api_key_menu_picker(&self.draft));
        } else {
            self.draft.api_key = None;
            self.provider_menu();
        }
    }

    fn enter_base_url(&mut self) {
        let provider = self
            .draft
            .provider
            .expect("provider chosen before base url");
        match provider.base_url_requirement() {
            crate::llm::BaseUrlRequirement::Required => {
                let initial = finalize::field_initial(
                    self.draft.base_url.as_deref(),
                    &self.existing,
                    self.existing_provider,
                    self.draft.provider,
                    |c| c.base_url.as_ref(),
                );
                self.screen = Screen::BaseUrlText(
                    TextInput::new(
                        "Base URL (e.g. http://localhost:1234/v1)",
                        initial.as_deref(),
                        false,
                        "base URL cannot be empty",
                    ),
                    false,
                );
            }
            crate::llm::BaseUrlRequirement::Optional(_) => {
                self.screen = Screen::BaseUrlMenu(base_url_menu_picker(&self.draft));
            }
            crate::llm::BaseUrlRequirement::None => self.provider_menu(),
        }
    }

    // -- rendering -------------------------------------------------------

    fn draw(&self, f: &mut ratatui::Frame) {
        let [header, hint, _, body] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(f.area());
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{MARGIN}aic setup — {}", self.section()),
                tstyle::plain(),
            ))),
            header,
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{MARGIN}{}", self.hint_line()),
                tstyle::muted(),
            ))),
            hint,
        );
        match &self.screen {
            Screen::ModeChoice(p) | Screen::TopMenu(p) | Screen::BackendChoice(p) => {
                p.draw(body, f)
            }
            Screen::ProviderPick(p) | Screen::ProviderMenu(p) => p.draw(body, f),
            Screen::ApiKeyMenu(p) | Screen::BaseUrlMenu(p) => p.draw(body, f),
            Screen::ApiKeyText(t, _) | Screen::BaseUrlText(t, _) | Screen::ModelText(t, _) => {
                t.draw(body, f)
            }
            Screen::ModelPick(p) | Screen::CliPick(p) | Screen::FailExit(p) => p.draw(body, f),
            Screen::Busy { label, .. } => f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(
                        "{MARGIN}{} {label}",
                        SPINNER[self.ticks as usize % SPINNER.len()]
                    ),
                    tstyle::pending(),
                ))),
                body,
            ),
            Screen::VerifyOutcome => f.render_widget(self.verify_paragraph(true), body),
            Screen::DiscardConfirm => f.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        format!("{MARGIN}? Discard unsaved changes?"),
                        tstyle::caution(),
                    )),
                    Line::from(Span::styled(
                        format!("{MARGIN}  Enter = discard · n = keep editing"),
                        tstyle::muted(),
                    )),
                ]),
                body,
            ),
        }
    }

    /// The Verify outcome / failure block. `pause` adds the
    /// press-Enter-to-continue row the success screen shows.
    fn verify_paragraph(&self, pause: bool) -> Paragraph<'static> {
        let mut lines = Vec::new();
        if let Some(smoke) = &self.smoke_line {
            lines.push(Line::from(Span::styled(
                format!("{MARGIN}{smoke}"),
                tstyle::muted(),
            )));
            lines.push(Line::from(""));
        }
        if let Some(block) = &self.verify {
            let title = match block.flow {
                ProbeFlow::Provider => format!(
                    "Verify — {} ({})",
                    block.target,
                    self.draft.effective_model()
                ),
                ProbeFlow::Cli => format!("Verify — {target}", target = block.target),
            };
            lines.push(Line::from(Span::styled(
                format!("{MARGIN}{title}"),
                tstyle::plain(),
            )));
            lines.push(Line::from(""));
            lines.extend(block.context.iter().cloned());
            if !block.context.is_empty() {
                lines.push(Line::from(""));
            }
            match (&block.reply, &block.error) {
                (Some(reply), _) => {
                    lines.push(Line::from(Span::styled(
                        format!("{MARGIN}{ICON_CHECK} Verified — the probe replied: {reply}"),
                        tstyle::success(),
                    )));
                }
                (_, Some(error)) => {
                    lines.push(Line::from(Span::styled(
                        format!("{MARGIN}{ICON_CROSS} Failed — the probe did not succeed."),
                        tstyle::failure(),
                    )));
                    lines.push(Line::from(Span::styled(
                        format!("{MARGIN}  Error: {error}"),
                        tstyle::muted(),
                    )));
                    for cause in &block.causes {
                        lines.push(Line::from(Span::styled(
                            format!("{MARGIN}  {cause}"),
                            tstyle::muted(),
                        )));
                    }
                }
                _ => {}
            }
        }
        if pause {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("{MARGIN}Press Enter to continue…"),
                tstyle::muted(),
            )));
        }
        Paragraph::new(lines)
    }

    fn section(&self) -> &str {
        match &self.screen {
            Screen::ModeChoice(_) | Screen::BackendChoice(_) => "choose a backend",
            Screen::TopMenu(_) | Screen::DiscardConfirm => "menu",
            Screen::ProviderPick(_)
            | Screen::ProviderMenu(_)
            | Screen::ApiKeyMenu(_)
            | Screen::ApiKeyText(..)
            | Screen::BaseUrlMenu(_)
            | Screen::BaseUrlText(..)
            | Screen::ModelPick(_)
            | Screen::ModelText(..) => "AI provider",
            Screen::CliPick(_) => "CLI agent",
            Screen::Busy { .. } | Screen::VerifyOutcome | Screen::FailExit(_) => "Verify",
        }
    }

    fn hint_line(&self) -> String {
        match &self.screen {
            Screen::ApiKeyText(..) | Screen::BaseUrlText(..) | Screen::ModelText(..) => {
                "Enter confirm · Esc back · Ctrl-C cancel".into()
            }
            Screen::VerifyOutcome => "Enter continue · Ctrl-C cancel".into(),
            Screen::DiscardConfirm => "Enter discard · n keep · Ctrl-C cancel".into(),
            Screen::Busy { .. } => "Esc abort · Ctrl-C cancel".into(),
            _ => "↑/↓ move · Enter confirm · Esc back · Ctrl-C cancel · type to filter".into(),
        }
    }
}

impl ProbeFlow {
    fn to_backend_kind(self) -> BackendKind {
        match self {
            ProbeFlow::Provider => BackendKind::Api,
            ProbeFlow::Cli => BackendKind::Cli,
        }
    }
}

enum PendingSnapshot {
    Probe,
    Models,
}

#[derive(Clone)]
enum JobKind {
    Fetch {
        provider: Provider,
        api_key: String,
        base_url: Option<String>,
    },
    ProbeProvider,
    ProbeCli,
    Smoke {
        program: String,
    },
}

fn is_cancel(key: KeyEvent) -> bool {
    key.modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
}

// -- row builders ---------------------------------------------------------

fn mode_choice_picker() -> Picker {
    Picker::new(
        "What would you like to configure?",
        vec![
            picker::item(vec![(
                "· API provider — use an API key (OpenAI, Anthropic, Gemini, …)",
                tstyle::plain(),
            )]),
            picker::item(vec![(
                "· CLI agent — reuse Claude Code / Codex / pi (no API key needed)",
                tstyle::plain(),
            )]),
        ],
    )
}

fn backend_choice_picker() -> Picker {
    let value = |v: &'static str| (v, tstyle::emphasis());
    Picker::new(
        "Which Backend should aic use?",
        vec![
            picker::item(vec![
                ("· API provider — ", tstyle::plain()),
                value("use an API key"),
            ]),
            picker::item(vec![
                ("· CLI agent — ", tstyle::plain()),
                value("reuse a coding-agent CLI (no API key)"),
            ]),
        ],
    )
}

fn top_menu_picker(draft: &Draft, existing: &Option<Config>, highlight: usize) -> Picker {
    let dirty = finalize::draft_dirty(draft, existing);
    let mut items = vec![
        picker::item(vec![
            ("· Backend — ", tstyle::plain()),
            (draft.active_backend().display_name(), tstyle::emphasis()),
        ]),
        menu_value_row("· AI provider — ", &provider_label(draft)),
        menu_value_row("· CLI agent — ", &cli_label(draft)),
        picker::item(vec![
            ("· Confirm before commit — ", tstyle::plain()),
            toggle_row(draft.confirm_before_commit),
        ]),
        picker::item(vec![
            ("· Review batch plan — ", tstyle::plain()),
            toggle_row(draft.review_batch_plan),
        ]),
    ];
    // Fold the latest verify outcome onto its backend's row (ADR 0020): no
    // standalone status line, the marker rides the row it is about.
    if let Some(status) = &draft.verify_status {
        let idx = match status.backend {
            BackendKind::Api => 1,
            BackendKind::Cli => 2,
        };
        let marker = if status.passed {
            (format!(" {ICON_CHECK}"), tstyle::success())
        } else {
            (format!(" {ICON_CROSS}"), tstyle::failure())
        };
        if let Some(row) = items.get_mut(idx) {
            row.line
                .spans
                .push(Span::styled(marker.0.clone(), marker.1));
            row.plain.push_str(&marker.0);
        }
    }
    let save = if dirty {
        "· Save & exit — unsaved changes"
    } else {
        "· Save & exit"
    };
    items.push(picker::item(vec![(save, tstyle::plain())]));
    Picker::new_at("What would you like to configure?", items, highlight)
}

fn menu_value_row(prefix: &str, value: &str) -> PickerItem {
    let placeholder = value == "(not set)" || value == "(not configured)";
    picker::item(vec![
        (prefix, tstyle::plain()),
        (
            value,
            if placeholder {
                tstyle::muted()
            } else {
                tstyle::emphasis()
            },
        ),
    ])
}

fn toggle_row(value: Option<bool>) -> (&'static str, ratatui::style::Style) {
    if value == Some(true) {
        ("on", tstyle::success())
    } else {
        ("off", tstyle::muted())
    }
}

fn provider_pick_picker(draft: &Draft, existing_provider: Option<Provider>) -> Picker {
    let items = Provider::all()
        .iter()
        .map(|p| {
            let model = preview_model(*p, draft);
            let badge_letter = p
                .display()
                .chars()
                .next()
                .unwrap_or('·')
                .to_uppercase()
                .to_string();
            let mut cells = vec![
                (badge_letter, tstyle::badge(p.name())),
                (format!(" {}  ", p.display()), tstyle::plain()),
            ];
            if model.is_empty() {
                cells.push(("(model not set)".into(), tstyle::muted()));
            } else {
                cells.push((format!("({model})"), tstyle::muted()));
            }
            picker::item(cells)
        })
        .collect();
    let cursor = draft
        .provider
        .or(existing_provider)
        .and_then(|ep| Provider::all().iter().position(|p| *p == ep))
        .unwrap_or(0);
    Picker::new_at("Choose your AI provider", items, cursor)
}

fn provider_menu_picker(draft: &Draft) -> Picker {
    Picker::new("Configure AI provider", provider_menu_rows(draft))
}

/// The applicable-field entries of the provider sub-menu (ADR 0019 steps)
/// plus `Done`, in row order. Pure.
fn provider_menu_entries(draft: &Draft) -> Vec<ProviderEntry> {
    let p = draft.provider.unwrap_or_default();
    let mut entries = Vec::new();
    for step in applicable_steps(p) {
        match step {
            super::provider::Step::Provider => {}
            super::provider::Step::ApiKey => entries.push(ProviderEntry::ApiKey),
            super::provider::Step::BaseUrl => entries.push(ProviderEntry::BaseUrl),
            super::provider::Step::Model => entries.push(ProviderEntry::Model),
        }
    }
    entries.push(ProviderEntry::Done);
    entries
}

fn provider_menu_rows(draft: &Draft) -> Vec<PickerItem> {
    let p = draft.provider.unwrap_or_default();
    let mut rows = Vec::new();
    for step in applicable_steps(p) {
        match step {
            super::provider::Step::Provider => {}
            super::provider::Step::ApiKey => {
                let (key, _) =
                    resolve_field(draft.api_key.as_deref().filter(|k| !k.is_empty()), "");
                rows.push(value_row("· API key — ", &api_key_label(&key)));
            }
            super::provider::Step::BaseUrl => {
                let (url, source) =
                    resolve_base_url(draft.base_url.as_deref().filter(|u| !u.is_empty()), &p);
                rows.push(value_row(
                    "· Base URL — ",
                    &base_url_label(url.as_deref(), source),
                ));
            }
            super::provider::Step::Model => {
                let model = draft
                    .model
                    .as_deref()
                    .filter(|m| !m.is_empty())
                    .unwrap_or("");
                rows.push(value_row("· Model — ", &model_label(model)));
            }
        }
    }
    rows.push(picker::item(vec![(
        "· Done — back to main menu",
        tstyle::plain(),
    )]));
    rows
}

/// A `· Label — value` row with the placeholder/values styling of the old
/// `styled_value` (emphasis for real values, muted for `(not set)`).
fn value_row(prefix: &str, value: &str) -> PickerItem {
    menu_value_row(prefix, value)
}

fn api_key_menu_picker(draft: &Draft) -> Picker {
    let key = effective_api_key(draft);
    let has_key = !key.is_empty();
    let mut rows = Vec::new();
    if has_key {
        rows.push(picker::item(vec![(
            format!(
                "{ICON_CHECK} Keep current key ({})",
                finalize::mask_key(&key)
            ),
            tstyle::plain(),
        )]));
    }
    rows.push(picker::item(vec![(
        "· No API key (keyless server)",
        tstyle::plain(),
    )]));
    rows.push(picker::item(vec![("· Enter a new key…", tstyle::plain())]));
    Picker::new("API key", rows)
}

fn base_url_menu_picker(draft: &Draft) -> Picker {
    let provider = draft.provider.expect("provider chosen before base url");
    let (effective, source) = resolve_base_url(
        draft.base_url.as_deref().filter(|u| !u.is_empty()),
        &provider,
    );
    let has_url = matches!(source, Source::Config);
    let default = match provider.base_url_requirement() {
        crate::llm::BaseUrlRequirement::Optional(d) => d,
        _ => "",
    };
    let current = effective.as_deref().unwrap_or(default);
    let mut rows = Vec::new();
    if has_url {
        rows.push(picker::item(vec![(
            format!("{ICON_CHECK} Keep current URL ({current})"),
            tstyle::plain(),
        )]));
    }
    rows.push(picker::item(vec![(
        format!("· Use default ({default})"),
        tstyle::plain(),
    )]));
    rows.push(picker::item(vec![("· Enter custom URL…", tstyle::plain())]));
    Picker::new("Base URL", rows)
}

fn cli_pick_picker() -> Picker {
    let rows = cli_menu_rows()
        .into_iter()
        .map(|row| match row {
            CliRow::Preset(name) => {
                let letter = name
                    .chars()
                    .next()
                    .unwrap_or('·')
                    .to_uppercase()
                    .to_string();
                picker::item(vec![
                    (letter, tstyle::badge(name)),
                    (format!(" {name}"), tstyle::plain()),
                ])
            }
            CliRow::Done => picker::item(vec![("· Done — back to main menu", tstyle::plain())]),
        })
        .collect();
    Picker::new("CLI agent backend", rows)
}

fn fail_exit_picker() -> Picker {
    Picker::new(
        "Probe failed",
        vec![
            picker::item(vec![("· Edit settings", tstyle::plain())]),
            picker::item(vec![("· Retry probe", tstyle::plain())]),
            picker::item(vec![("· Save anyway", tstyle::plain())]),
        ],
    )
}

// -- small pure helpers ---------------------------------------------------

/// The effective (draft or seeded-config) API key, unmasked.
fn effective_api_key(draft: &Draft) -> String {
    resolve_field(draft.api_key.as_deref().filter(|k| !k.is_empty()), "").0
}

/// The cached model list under `key`, if this session already fetched one.
fn cache_sentinel(draft: &Draft) -> String {
    let p = draft.provider.unwrap_or_default();
    let (api_key, _) = resolve_field(draft.api_key.as_deref().filter(|k| !k.is_empty()), "");
    let (base_url, _) = resolve_base_url(draft.base_url.as_deref().filter(|u| !u.is_empty()), &p);
    cache_key(p, base_url.as_deref(), &api_key)
}

/// Whether picker row `i` of the model screen is the `Custom model…`
/// escape hatch (always the last row).
fn is_custom_row(draft: &Draft, i: usize) -> bool {
    let p = draft.provider.unwrap_or_default();
    let (api_key, _) = resolve_field(draft.api_key.as_deref().filter(|k| !k.is_empty()), "");
    let (base_url, _) = resolve_base_url(draft.base_url.as_deref().filter(|u| !u.is_empty()), &p);
    let models = draft
        .model_cache
        .get(&cache_key(p, base_url.as_deref(), &api_key))
        .cloned()
        .unwrap_or_default();
    let rows = picker_rows(&models);
    i == rows.len()
}

/// The model list entry at picker row `i` (all rows but the last are
/// models; the last is the custom escape hatch, handled by [`is_custom_row`]).
fn model_at(draft: &Draft, i: usize) -> String {
    let p = draft.provider.unwrap_or_default();
    let (api_key, _) = resolve_field(draft.api_key.as_deref().filter(|k| !k.is_empty()), "");
    let (base_url, _) = resolve_base_url(draft.base_url.as_deref().filter(|u| !u.is_empty()), &p);
    let models = draft
        .model_cache
        .get(&cache_key(p, base_url.as_deref(), &api_key))
        .cloned()
        .unwrap_or_default();
    models.get(i).cloned().unwrap_or_default()
}

fn clone_draft(draft: &Draft) -> Draft {
    draft.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::CliConfig;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn app_with_hanging_fetch() -> App {
        let deps = Arc::new(SetupDeps {
            fetch_models: Box::new(|_, _, _| {
                std::thread::sleep(std::time::Duration::from_secs(2));
                Vec::new()
            }),
            verify_provider: Box::new(|_| panic!("verify_provider not scripted")),
            verify_cli: Box::new(|_| panic!("verify_cli not scripted")),
        });
        let draft = Draft {
            provider: Some(Provider::OpenAI),
            api_key: Some("sk-test".into()),
            ..Default::default()
        };
        let mut app = App::new(deps, None, None, draft);
        app.screen = Screen::ProviderMenu(provider_menu_picker(&app.draft));
        app
    }

    /// Esc on the Busy screen aborts the job: the pending channel is
    /// dropped (the finishing thread's send becomes a harmless no-op) and
    /// the screen the job started from is restored — a wedged network call
    /// can never trap the user on the spinner.
    #[test]
    fn busy_esc_aborts_job_and_restores_back_screen() {
        let mut app = app_with_hanging_fetch();
        app.enter_model_step(); // cache miss + key present → Fetch job + Busy
        assert!(matches!(app.screen, Screen::Busy { .. }));
        assert!(app.pending.is_some());
        match app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)) {
            Ok(Flow::Running) => {}
            other => panic!("Esc on Busy must keep the wizard running, got {other:?}"),
        }
        assert!(app.pending.is_none(), "job must be detached");
        assert!(
            matches!(app.screen, Screen::ProviderMenu(_)),
            "must return to the provider menu"
        );
    }

    /// A finished job's result must survive the poll → on_job handoff:
    /// `poll_job` used to clear `pending` before `on_job` read the kind, so
    /// every completed fetch/probe was dropped and the wizard stayed on the
    /// Busy spinner forever — the "loading, cannot exit" regression.
    #[test]
    fn poll_job_result_is_not_dropped_before_on_job() {
        let mut app = App::new(
            Arc::new(SetupDeps {
                fetch_models: Box::new(|_, _, _| vec!["gpt-test".into()]),
                verify_provider: Box::new(|_| panic!("not scripted")),
                verify_cli: Box::new(|_| panic!("not scripted")),
            }),
            None,
            None,
            Draft {
                provider: Some(Provider::OpenAI),
                api_key: Some("sk-test".into()),
                ..Default::default()
            },
        );
        app.screen = Screen::ProviderMenu(provider_menu_picker(&app.draft));
        app.enter_model_step(); // → Fetch job + Busy
        assert!(matches!(app.screen, Screen::Busy { .. }));
        // Drain the (already finished) job like the run loop does.
        let mut done = None;
        for _ in 0..50 {
            if let Some(d) = app.poll_job() {
                done = Some(d);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let done = done.expect("job result must arrive");
        assert!(matches!(app.on_job(done), Ok(Flow::Running)));
        assert!(
            matches!(app.screen, Screen::ModelPick(_)),
            "a fetched model list must land on the picker, got stuck instead"
        );
    }

    /// Ctrl-C cancels the wizard from the Busy screen too — the guard used
    /// to spare Busy, which made a hung probe an unkillable spinner.
    #[test]
    fn busy_ctrl_c_cancels() {
        let mut app = app_with_hanging_fetch();
        app.enter_model_step();
        match app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
            Ok(Flow::Cancel) => {}
            other => panic!("Ctrl-C on Busy must cancel, got {other:?}"),
        }
    }

    /// A Busy spawned from a Busy (the smoke→probe chain) keeps the
    /// original back screen, not the intermediate spinner.
    #[test]
    fn busy_from_busy_keeps_original_back() {
        let mut app = App::new(
            Arc::new(SetupDeps {
                fetch_models: Box::new(|_, _, _| Vec::new()),
                verify_provider: Box::new(|_| panic!("not scripted")),
                verify_cli: Box::new(|_| Ok("ok".into())),
            }),
            None,
            None,
            Draft {
                cli: CliConfig {
                    command: Some("true".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        app.screen = Screen::CliPick(cli_pick_picker());
        app.probe_cli(Some("true".into())); // Busy(back=CliPick)
        // The smoke result arrives; the chained probe Busy must still
        // remember CliPick as its back screen.
        let _ = app.on_job(JobDone::Smoke("✓ `true` found".into())).unwrap();
        match &app.screen {
            Screen::Busy { back, .. } => {
                assert!(
                    matches!(back.as_ref(), Screen::CliPick(_)),
                    "chained Busy must keep the original back screen"
                );
            }
            Screen::ModelPick(_) | Screen::VerifyOutcome | Screen::FailExit(_) => {
                panic!("expected chained Busy")
            }
            _ => panic!("expected chained Busy"),
        }
    }
}
