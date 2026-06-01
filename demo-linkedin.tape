Output demo-linkedin.gif

Set FontSize 16
Set Width 1000
Set Height 600
Set Padding 20
Set Theme "Dracula"
Set Margin 20
Set MarginFill "#282a36"
Set BorderRadius 10


# 1. Start ollamaMQ with multi-backend setup
Type "cargo run -- --backend-urls http://localhost:11434,http://127.0.0.1:1234"
Sleep 500ms
Enter

# 2. Wait for first health check
Sleep 1s

# 3. Showcase Model Awareness (Active/Last model)
# (Imagine background traffic here)
Sleep 1s

# 4. Interactive Model Discovery
Type "h" # Switch to Backends
Sleep 1s
Type " " # Expand first backend
Sleep 1s
Type "j" # Move to second
Sleep 500ms
Type " " # Expand second
Sleep 2s

# 5. Highlight "In RAM" indicators
# Models in memory will be green and bold
Sleep 1s

# 6. Help overlay
Type "?"
Sleep 1s

# Exit
Type "q"
