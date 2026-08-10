Output "docs/demo.gif"

Set FontSize 24
Set Width 1200
Set Height 880
Set Padding 20
Set Theme {
  "background": "#0d1117",
  "foreground": "#c9d1d9",
  "cursor": "#58a6ff"
}

# Set up demo repo + fake aic (hidden)
Hide
Type "source demo/setup.sh"
Enter
Sleep 2s
Show

# Show the actual changes so viewer sees what aic will split
Type@30ms "git diff"
Sleep 200ms
Enter
Sleep 3s

# Run aic — the money shot
Type@50ms "aic"
Sleep 150ms
Enter
Sleep 11s
