Output "docs/resolve-demo.gif"

Set Shell "zsh"
Set FontFamily "JetBrainsMono Nerd Font"
Set FontSize 22
Set Width 1200
Set Height 720
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

# Set up ~/aic-resolve repo with a real merge conflict (hidden)
Hide
Sleep 1s
Type "unset http_proxy https_proxy all_proxy; export GIT_PAGER=cat PAGER=cat; bash demo/resolve-setup.sh; alias aic=~/aic-resolve/bin/aic"
Enter
Sleep 3s
Type "cd ~/aic-resolve"
Enter
Sleep 1s
Type "clear"
Enter
Sleep 500ms
Show

# Run aic resolve — real binary, real LLM
Type@50ms "aic resolve"
Sleep 150ms
Enter
Sleep 9s

# Accept the first proposed resolution (Enter = yes)
Enter
Sleep 3s

# Accept the second proposed resolution
Enter
Sleep 5s
