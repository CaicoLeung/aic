Output "docs/resolve-demo.gif"

Set FontSize 24
Set Width 1200
Set Height 700
Set Padding 20
Set Theme {
  "background": "#0d1117",
  "foreground": "#c9d1d9",
  "cursor": "#58a6ff"
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
