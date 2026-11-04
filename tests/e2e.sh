#!/usr/bin/env bash
# End-to-end check: drives the real binary in an isolated tmux server, so personal
# tmux and terminal keybindings are not involved. Usage: tests/e2e.sh
set -u
cd "$(dirname "$0")/.." || exit 1
cargo build -q || exit 1
BIN=$PWD/target/debug/kalipso
T=(tmux -L "kalipso-test-$$" -f /dev/null)
WORK=$(mktemp -d)
CONFIG=$(mktemp -d)
python3 tests/mock_llm.py "$CONFIG/port" &
MOCK=$!
trap '"${T[@]}" kill-server 2>/dev/null; kill "$MOCK" 2>/dev/null; rm -rf "$WORK" "$CONFIG"' EXIT
for i in $(seq 50); do [ -s "$CONFIG/port" ] && break; sleep 0.1; done
FAILED=0

screen() { "${T[@]}" capture-pane -p -t kalipso; }
key() { "${T[@]}" send-keys -t kalipso "$@"; }
pass() { echo "ok    $1"; }
fail() { echo "FAIL  $1"; screen | sed 's/^/      | /'; FAILED=1; }

# Wait up to 5s for text to appear, or to disappear with `gone`.
expect() {
    local name=$1 text=$2 i
    for i in $(seq 50); do
        screen | grep -qF -- "$text" && { pass "$name"; return; }
        sleep 0.1
    done
    fail "$name (expected: $text)"
}
gone() {
    local name=$1 text=$2 i
    for i in $(seq 50); do
        screen | grep -qF -- "$text" || { pass "$name"; return; }
        sleep 0.1
    done
    fail "$name (still showing: $text)"
}
# Status line starts with the focused pane's title.
focus() {
    local name=$1 title=$2 i
    for i in $(seq 50); do
        screen | tail -1 | grep -qF -- "$title" && { pass "$name"; return; }
        sleep 0.1
    done
    fail "$name (expected focus: $title)"
}

# Fixture project.
mkdir -p "$WORK/sub"
printf 'fn main() {\n    println!("hello");\n}\n' > "$WORK/hello.rs"
printf 'nested\n' > "$WORK/sub/nested.txt"
printf 'ignored.txt\n' > "$WORK/.gitignore"
printf 'secret\n' > "$WORK/ignored.txt"

# `env`, because tmux overwrites SHELL in new panes. Plain sh keeps results independent of
# anyone's shell config. Only Claude has a key, pointed at the mock, and the real config is
# left alone.
"${T[@]}" new-session -d -s kalipso -x 140 -y 45 -c "$WORK" env SHELL=/bin/sh \
    ANTHROPIC_API_KEY=e2e-test-key ANTHROPIC_BASE_URL="http://127.0.0.1:$(cat "$CONFIG/port")/v1" \
    XDG_CONFIG_HOME="$CONFIG" OPENAI_API_KEY= XAI_API_KEY= OPENROUTER_API_KEY= "$BIN"
expect "starts with all panes" "1 Files"
expect "terminal pane visible" "2 Terminal"
expect "editor pane visible" "3 Editor"
expect "chat pane visible" "4 Chat"
focus "files focused at start" "1 Files"

key M-1; focus "Alt+1 focuses files" "1 Files"
key M-2; focus "Alt+2 focuses terminal" "2 Terminal"
key M-3; focus "Alt+3 focuses editor" "3 Editor"
key M-4; focus "Alt+4 focuses chat" "4 Chat"
key M-Left; focus "Alt+Left from chat reaches editor" "3 Editor"
key M-Down; focus "Alt+Down reaches terminal" "2 Terminal"
key M-Right; focus "Alt+Right reaches chat" "4 Chat"
key M-Left; focus "Alt+Left returns to last middle pane" "2 Terminal"
key M-Up; focus "Alt+Up reaches editor" "3 Editor"
key M-Left; focus "Alt+Left reaches files" "1 Files"
key M-Right; focus "Alt+Right returns to editor" "3 Editor"

key M-z; gone "Alt+Z hides other panes" "1 Files"
key M-z; expect "Alt+Z restores layout" "1 Files"

# --- Files and Editor
key M-1
expect "tree lists files" "hello.rs"
expect "tree lists folders" "sub"
if screen | grep -qF "ignored.txt"; then fail "gitignored file hidden"; else pass "gitignored file hidden"; fi
key .; expect "dot toggles ignored files" "ignored.txt"
key .; gone "dot hides them again" "ignored.txt"
key Right; expect "Right expands a folder" "nested.txt"
key Left
key Down; key Enter
focus "Enter opens file in editor" "3 Editor"
expect "file content shown" 'println!("hello")'
key -l 'XYZ'
expect "typing edits the buffer" 'XYZfn main'
expect "tab marks unsaved changes" "hello.rs ●"
key C-z; gone "undo removes the edit" 'XYZfn'
key -l 'ABC'; key C-s
gone "save clears the unsaved mark" "hello.rs ●"
if head -c 12 "$WORK/hello.rs" | grep -qF 'ABCfn main'; then pass "save writes to disk"; else fail "save writes to disk"; fi

printf 'changed outside\n' > "$WORK/hello.rs"
expect "clean buffer reloads outside changes" "changed outside"
key -l 'mine '
printf 'changed again\n' > "$WORK/hello.rs"
expect "dirty buffer shows conflict banner" "Changed on disk"
if screen | grep -qF "mine "; then pass "dirty buffer keeps your edits"; else fail "dirty buffer keeps your edits"; fi
key C-s; expect "save asks before overwriting" "Overwrite it with your version?"
key n
if grep -qF 'changed again' "$WORK/hello.rs"; then pass "declining keeps the file on disk"; else fail "declining keeps the file on disk"; fi
key C-r; expect "Ctrl+R reloads from disk" "changed again"
gone "reload clears the banner" "Changed on disk"

key C-f; expect "Ctrl+F asks what to find" "Find"
key -l 'again'; key Enter; gone "find popup closes" " Find "

key M-t; expect "Alt+T opens an untitled tab" "untitled"
key -l 'draft'; key M-w; expect "closing unsaved tab asks" "Discard unsaved changes"
key y; gone "y closes the tab" "untitled"

key M-1; key n; expect "n prompts for a name" "New file"
key -l 'made/by/test.txt'; key Enter
focus "new file opens in editor" "3 Editor"
if [ -f "$WORK/made/by/test.txt" ]; then pass "new file created with parents"; else fail "new file created with parents"; fi
expect "tree reveals the new file" "test.txt"
key M-1; key d; expect "delete asks first" "Delete made/by/test.txt?"
key y; gone "tree drops deleted file" "test.txt ●"
if [ -e "$WORK/made/by/test.txt" ]; then fail "delete removes the file"; else pass "delete removes the file"; fi

# --- Terminal
key M-2
focus "terminal focused" "2 Terminal"
key -l 'echo sum=$((40+2))'; key Enter
expect "shell runs commands" "sum=42"
key -l 'pwd > pwd.txt'; key Enter
for i in $(seq 30); do [ -s "$WORK/pwd.txt" ] && break; sleep 0.1; done
if [ "$(cat "$WORK/pwd.txt" 2>/dev/null)" = "$(cd "$WORK" && pwd -P)" ]; then pass "shell starts in the project root"; else fail "shell starts in the project root"; fi
expect "new files appear in the tree" "pwd.txt"
key -l 'sleep 30'; key Enter
key C-c
key -l 'echo after=$((1+1))'; key Enter
expect "Ctrl+C reaches the program" "after=2"
key M-t; expect "Alt+T opens a second terminal" "term 2"
key -l 'echo second=$((2*2))'; key Enter
expect "second terminal works" "second=4"
gone "tabs have separate screens" "after=2"
key M-PPage; expect "Alt+PgUp switches back" "after=2"
key -l 'seq 1 3000'; key Enter
expect "output flood finishes" "3000"
key M-s; expect "Alt+S shows scroll mode" "SCROLL MODE"
key PPage; expect "scroll mode reads scrollback" "2990"
key Escape; gone "Esc leaves scroll mode" "SCROLL MODE"
key -l 'vim -u NONE -c "set noswapfile" -c "normal ivim works" scratch.txt'; key Enter
expect "full-screen programs render" "vim works"
key -l ':q!'; key Enter
key -l 'echo vimdone=$((5+5))'; key Enter
expect "shell is back after a full-screen program" "vimdone=10"
key M-Left; focus "Alt+Left leaves the terminal even mid-session" "1 Files"
key M-2
key -l 'exit'; key Enter
gone "shell exit closes its tab" "term 1"
key M-w; expect "closing the last tab shows a hint" "No terminal open"
key M-t; key -l 'echo back=$((3+3))'; key Enter
expect "Alt+T starts a new shell" "back=6"

# --- Chat and agents, against the mock provider
key M-4
focus "chat focused" "4 Chat"
expect "welcome lists agents" "Commit: inspect the diff"
expect "welcome shows the provider with a key" "✓ Claude"
expect "welcome names the missing keys" "set OPENAI_API_KEY"
key Enter; expect "Enter opens the agent picker" "New chat: pick an agent"
key -l 'zzz'; expect "a filter can match nothing" "(no matches)"
key BSpace; key BSpace; key BSpace; gone "clearing the filter restores the list" "(no matches)"
key -l 'revi'; key Enter
expect "filtering picks the right agent, and one provider skips the provider picker" "Review 1"
key M-w; gone "Alt+W closes an idle chat" "Review 1"
key Enter; key Enter
expect "the first agent is selected by default" "Code 2"
expect "Claude gets its default model" "claude-opus-5-5"

key -l 'make a file'; key C-j; key -l 'on two lines'
expect "Ctrl+J adds a line to the input" "on two lines"
key Enter
expect "thinking appears in the transcript" "I should write the file."
expect "assistant text streams in" "Writing it now."
expect "approval shows the change" "Create agent.txt"
expect "approval shows the new contents" "+made by agent"
expect "approval prompt appears" "Allow this?"
expect "tab shows it needs attention" "Code 2 !"
if [ -e "$WORK/agent.txt" ]; then fail "nothing written before approval"; else pass "nothing written before approval"; fi
key -l 'x'
if [ -e "$WORK/agent.txt" ]; then fail "other keys don't approve"; else pass "other keys don't approve"; fi
key M-1
expect "other panes warn about the approval" "An agent is waiting for your approval"
key M-4
key y
expect "tool result shows in the transcript" "✓ write_file agent.txt"
expect "the agent loop continues after the tool" "All done. Here is the code:"
expect "fenced code is shown" "fn main() {}"
expect "token counts update" "123 in 90 out"
if [ "$(cat "$WORK/agent.txt" 2>/dev/null)" = "made by agent" ]; then pass "approved write reaches the disk"; else fail "approved write reaches the disk"; fi
expect "the tree shows the agent's file" "agent.txt"
gone "the tab mark clears when done" "Code 2 !"

key -l 'run a command'; key Enter
expect "commands show before they run" 'echo key=[$ANTHROPIC_API_KEY]'
key n
expect "a declined command is reported" "✗ run_command"
if [ -e "$WORK/command.txt" ]; then fail "a declined command never runs"; else pass "a declined command never runs"; fi
key -l 'run a command'; key Enter
expect "the command asks again" "Allow this?"
key y
for i in $(seq 50); do [ -s "$WORK/command.txt" ] && break; sleep 0.1; done
if grep -qxF 'key=[]' "$WORK/command.txt" 2>/dev/null; then pass "commands don't see API keys"; else fail "commands don't see API keys"; fi
gone "the approval prompt clears" "Allow this?"

key -l '/model'; key Enter
expect "/model lists the provider's models" "Claude model"
key -l 'sonnet'; key Enter
expect "/model switches the model" "claude-sonnet-mock"
if grep -qF 'claude-sonnet-mock' "$CONFIG/kalipso/config.json" 2>/dev/null; then pass "the model choice is saved"; else fail "the model choice is saved"; fi
key -l 'hello there'; key Enter
expect "a second message gets a reply" "Hello from the mock."

key M-t; expect "Alt+T in chat opens the picker" "New chat: pick an agent"
key -l 'design'; key Enter
expect "a second chat tab opens" "Design 3"
gone "a new chat starts empty" "Hello from the mock."
key M-PPage; expect "Alt+PgUp returns to the other chat's transcript" "Hello from the mock."
key M-PPage; gone "Alt+PgUp wraps around" "Hello from the mock."
key M-w; gone "Alt+W closes a chat tab" "Design 3"

key M-z; expect "chat can zoom" "Hello from the mock."
gone "zoom hides the file tree" "1 Files"
key M-z; expect "zoom restores" "1 Files"

# --- end of scenario: quitting must hand back a working terminal.
"${T[@]}" set-option -t kalipso remain-on-exit on >/dev/null
key C-q
for i in $(seq 50); do
    [ "$("${T[@]}" display-message -p -t kalipso '#{pane_dead}')" = 1 ] && break
    sleep 0.1
done
if [ "$("${T[@]}" display-message -p -t kalipso '#{pane_dead_status}')" = 0 ]; then
    pass "Ctrl+Q exits cleanly"
else
    fail "Ctrl+Q exits cleanly"
fi

[ "$FAILED" = 0 ] && echo "all e2e checks passed"
exit "$FAILED"
