Output "docs/resolve-demo.gif"

Set FontFamily "JetBrainsMonoNL Nerd Font"
Set FontSize 22
Set Width 1200
Set Height 660
Set Padding 24
Set Theme {
  "background": "#1e1e2e",
  "foreground": "#cdd6f4",
  "cursor": "#f5e0dc",
  "selection_background": "#585b70",
  "selection_foreground": "#cdd6f4",
  "0": "#45475a",
  "1": "#f38ba8",
  "2": "#a6e3a1",
  "3": "#f9e2af",
  "4": "#89b4fa",
  "5": "#f5c2e7",
  "6": "#94e2d5",
  "7": "#bac2de",
  "8": "#585b70",
  "9": "#f38ba8",
  "10": "#a6e3a1",
  "11": "#f9e2af",
  "12": "#89b4fa",
  "13": "#f5c2e7",
  "14": "#94e2d5",
  "15": "#a6adc8"
}

# Set up demo repo with merge conflict (hidden)
Hide
Type "source demo/resolve-setup.sh"
Enter
Sleep 2s
Show

# Run aic resolve — straight in, no git status clutter
Type@50ms "aic resolve"
Sleep 150ms
Enter
Sleep 12s
