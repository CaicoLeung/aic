use anyhow::Result;
use self_update::cargo_crate_version;

pub fn run_update() -> Result<()> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner("CaicoLeung")
        .repo_name("aic")
        .bin_name("aic")
        .show_download_progress(true)
        .current_version(cargo_crate_version!())
        .build()?
        .update()?;
    match status {
        self_update::Status::UpToDate(_) => {
            println!("Already up to date (v{})", cargo_crate_version!())
        }
        self_update::Status::Updated(v) => println!("Updated to version {v}"),
    }
    Ok(())
}
