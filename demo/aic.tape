Output "docs/demo.gif"

Set FontSize 16
Set Width 820
Set Height 440
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

# Show working-tree changes
Type@30ms "git diff --stat"
Sleep 200ms
Enter
Sleep 1500ms

# Run aic — the money shot
Type@50ms "aic"
Sleep 150ms
Enter
Sleep 5s
