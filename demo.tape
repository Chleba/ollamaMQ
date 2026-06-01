Output demo.gif

Set FontSize 14
Set Width 1200
Set Height 720
Set Padding 10
Set Theme "Dracula"

# Start ollamaMQ with multiple backends
Type "cargo run -- --backend-urls http://localhost:11434,http://localhost:1234"
Sleep 500ms
Enter

# Wait for UI to stabilize and health checks to run
Sleep 1s

# Show help overlay
Type "?"
Sleep 1s

# Switch to Backends panel (h to go left from Users)
Sleep 1s
Type "h"
Sleep 1s
Type "?"

# Navigate backends and expand models
Type "j"
Sleep 500ms
Type " "
Sleep 2s
Type "k"
Sleep 500ms
Type " "
Sleep 2s

# Switch back to Users panel (l to go right from Backends)
Type "l"
Sleep 1s

# Navigate through active users
Type "j"
Sleep 300ms
Type "j"
Sleep 300ms
Type "k"

# Demonstrate Boost and VIP features
Sleep 1s
Type "b"
Sleep 1.5s
Type "j"
Type "p"
Sleep 2s

# Switch to Blocked Items panel (l to go right from Users)
Type "l"
Sleep 1s
Type "h"

# Stay a bit to show activity
Sleep 2s

# Exit
Type "q"
