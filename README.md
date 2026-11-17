# zen

A terminal text editor with nothing in it but a cursor. No status bar, no line
numbers, no syntax highlighting, no hotkeys.

- **One binary.** Statically linked, no dependencies beyond the Rust standard
  library.
- **One source file.** All code is in `src/main.rs`.
- **One config file.** Every setting and every key sequence is in `config.toml`.

## Build and run

```
cargo build --release
./target/release/zen notes.txt
some-command | ./target/release/zen        # edit piped text
```

Copy `target/release/zen` anywhere on your `PATH`.

## How commands work

There are no Ctrl or Alt shortcuts. A command is a **sequence of ordinary
keys** typed quickly, for example `;w` to save. The keys show up as text while
you type them; when the sequence completes they vanish and the command runs.

- Typed slower than `sequence_timeout` (500 ms by default), a sequence is
  just text. That is how you write a literal `;w`.
- A sequence is matched wherever it occurs, including inside a word. If you
  set `save = "evera"`, typing "several" quickly saves.
- The bottom row is used only while a command asks something (`find:`,
  `save as:`) or for a warning, and is empty again on the next key.

Arrow keys, Home/End, PageUp/PageDown, Ctrl+arrows (by word), Shift+arrows
(select), Backspace/Delete and Alt+Backspace (delete word) work as usual.

## Configuration

`config.toml` in this repository is compiled into the binary as the defaults.
To change anything without rebuilding:

```
mkdir -p ~/.config/zen
zen --print-config > ~/.config/zen/config.toml
```

Then edit that file. You only need to keep the lines you change; everything
else falls back to the default. `zen --config other.toml` uses a different
file. A mistake in the file is reported with its line number when zen starts.

Font and font size are not here: a terminal program cannot change them. Set
them in your terminal emulator.

### Switching features on and off

Every feature has a setting. Change the value and restart zen.

| Feature | Setting | Default | To switch |
|---|---|---|---|
| Typewriter scrolling | `typewriter` | `false` (off) | `typewriter = true` |
| Focus dimming | `focus` | `"off"` | `focus = "paragraph"`, `"sentence"` or `"line"` |
| Mouse | `mouse` | `true` (on) | `mouse = false` |
| Desktop clipboard | `system_clipboard` | `true` (on) | `system_clipboard = false` |
| Crash recovery | `recovery` | `true` (on) | `recovery = false` |
| Autosave | `autosave` | `false` (off) | `autosave = true` |
| Changed-on-disk check | `check_disk` | `true` (on) | `check_disk = false` |
| Remember position | `remember_position` | `true` (on) | `remember_position = false` |
| Warnings on the bottom row | `messages` | `true` (on) | `messages = false` |
| Auto-indent | `auto_indent` | `true` (on) | `auto_indent = false` |
| Newline at end of file | `final_newline` | `true` (on) | `final_newline = false` |
| Spaces instead of tabs | `soft_tabs` | `false` (off) | `soft_tabs = true` |
| Centred text column | `center` | `true` (on) | `center = false` |
| Any single command | its line under `[keys]` | see below | `find = ""` |
| A text expansion | its line under `[expand]` | see below | `";;date" = ""` |
| A shell command | a line under `[shell]` | none defined | add or remove the line |

Typewriter scrolling and focus dimming can also be flipped while writing,
with `;t` and `;o`. That lasts until you quit; the config decides how zen
starts.

### Page and margins

```toml
margin_top = 1        # empty rows above the text
margin_bottom = 1     # empty rows below
margin_left = 2       # empty columns to the left
margin_right = 2      # empty columns to the right
width = 0             # widest the text column may get; 0 = all the room
center = true         # centre the column when width leaves room over
```

By default the text uses the whole window inside the margins. For a narrow
column in the middle of a wide window, set `width = 80`. For a narrow column
against the left edge, also set `center = false`. All four margins can be `0`.

### Cursor and colours

```toml
cursor = "bar"            # "block", "bar" or "underline"
cursor_blink = false
cursor_color = ""         # "#rrggbb"; "" = the terminal's own
foreground = ""           # text colour
background = ""
dim_color = ""            # dimmed text in focus mode; "" = terminal's faint
```

### Writing feel

- **Typewriter scrolling** (`typewriter = true`): the line you write on stays
  at one height and the text moves instead. `typewriter_position` chooses the
  height: `0` is the top of the page, `50` the middle, `100` the bottom.
- **Focus dimming** (`focus`): everything except the current paragraph,
  sentence or line is dimmed. If your terminal does not draw faint text, set
  `dim_color` to a grey such as `"#555555"`.
- **Reflow** (`;j`): re-wraps the paragraph under the cursor with real line
  breaks, at `reflow_width` columns (`0` = the page width).

### Clipboard

With `system_clipboard = true`, copy, cut and paste use the desktop
clipboard. zen uses the first of these that is installed: `wl-copy`/`wl-paste`
on Wayland, then `xclip`, then `xsel`. It also asks the terminal itself, which
works over SSH.

- Copied text stays available after zen, or its terminal window, is closed.
- If the desktop clipboard holds no text (an image, say), paste falls back to
  the last thing copied inside zen.
- A clipboard program that does not answer within 2 seconds is given up on,
  so paste can never freeze the editor.
- `xsel` occasionally loses very large copies on its own; install `xclip` or
  use Wayland's `wl-clipboard` if you copy hundreds of kilobytes at a time.

If your setup needs something else, name the programs:

```toml
copy_command = "my-copy-tool"      # receives the text on its input
paste_command = "my-paste-tool"    # prints the clipboard
```

With `system_clipboard = false`, zen keeps a clipboard of its own.

### Safety

- **Crash recovery** (`recovery`): while there are unsaved changes, zen
  writes them every `recovery_interval` seconds to a hidden file beside
  yours (`.name.zen-recover`; for a text with no name yet,
  `~/.local/state/zen/unnamed.zen-recover`). It is removed when you save or
  quit with `;Q`. If zen is killed or the terminal closes, the next start
  asks whether to bring the text back.
- **Autosave** (`autosave = true`): saves by itself once you have paused for
  `autosave_delay` seconds. Only for files that already have a name.
- **Changed-on-disk check** (`check_disk`): if another program changed the
  file since you opened it, or *save as* names a file that already exists,
  the save is refused with a warning. Save a second time to overwrite.
- **Remember position** (`remember_position`): a file reopens where you left
  it. Positions are kept in `~/.local/state/zen/positions`.

### Key sequences

Under `[keys]`, each command has a sequence. Any text works, and a list gives
alternatives:

```toml
[keys]
save = "evera"
quit = [";q", "byebye"]
find = ""                 # switched off
```

No sequence may be the beginning of another (`;d` and `;date` cannot both
exist); zen refuses to start and names the pair.

| Command | Default | What it does |
|---|---|---|
| `save` | `;w` | save |
| `save_as` | `;W` | save under a new name |
| `quit` | `;q` | quit (refuses if there are unsaved changes) |
| `force_quit` | `;Q` | quit and throw away unsaved changes |
| `save_quit` | `;x` | save and quit |
| `undo` / `redo` | `;u` / `;r` | |
| `find` | `;f` | find; all-lowercase searches ignore case |
| `find_next` / `find_prev` | `;n` / `;N` | next / previous match |
| `replace` | `;h` | find and replace: `y` yes, `n` skip, `a` all, `q` stop |
| `goto_line` | `;g` | go to a line number |
| `select` | `;v` | start selecting, then move; again to stop |
| `select_all` | `;a` | |
| `copy` / `cut` / `paste` | `;c` / `;d` / `;p` | |
| `delete_line` | `;k` | |
| `duplicate_line` | `;y` | |
| `line_up` / `line_down` | `;[` / `;]` | move the current line |
| `paragraph_up` / `paragraph_down` | `;{` / `;}` | jump to the blank line above / below |
| `reflow` | `;j` | re-wrap the current paragraph |
| `word_count` | `;#` | words and characters (of the selection, if any) |
| `typewriter` | `;t` | typewriter scrolling on / off |
| `focus` | `;o` | focus dimming on / off |

### Text expansions

Under `[expand]`, a sequence inserts text instead of running a command:

```toml
[expand]
";;date" = "{date}"                  # 2026-11-17
";;time" = "{time}"                  # 14:05
";;long" = "{%A, %d %B %Y}"          # any strftime format
";;sig"  = "Kind regards,\nA. Writer"
```

Set one to `""` to remove it.

### Shell commands

Under `[shell]`, a sequence sends the selection (or the whole text when
nothing is selected) through a program and replaces it with the output:

```toml
[shell]
";;sort" = "sort"
";;up"   = "tr a-z A-Z"
```

If the program fails, the text is left alone and its complaint is shown. A program
still running after 30 seconds is stopped.

## Limits

- Files must be UTF-8 text; anything else is refused rather than risked.
- Linux only.
