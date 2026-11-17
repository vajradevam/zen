# zen

A terminal text editor with nothing in it but a cursor.

No status bar, no line numbers, no syntax highlighting, no menus, no hotkeys.
You open a file and there is your text, and a cursor that writes.

- **Nothing on screen but your words.** The bottom row is borrowed only while
  a command asks you something, and is empty again on the next key.
- **Commands are typed, not chorded.** Saving is `;w` typed quickly. Any
  sequence of ordinary keys can be a command, and all of them are yours to
  change.
- **One binary.** Statically linked, 1.6 MB, nothing to install beside it.
- **One source file, no dependencies.** All code is in `src/main.rs` and uses
  only the Rust standard library.
- **One config file.** Every setting, sequence, text expansion and shell
  command lives in `config.toml`, separate from the code.

Under the empty screen there is a complete editor: undo and redo, find and
replace, selection, desktop clipboard, mouse, soft word wrap, typewriter
scrolling, focus dimming, crash recovery and autosave.

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Commands](#commands)
- [Configuration](#configuration)
- [Files zen writes](#files-zen-writes)
- [Limits](#limits)
- [Licence](#licence)

## Install

zen runs on Linux. Building it needs [Rust](https://rustup.rs) 1.82 or newer
and `make`.

```sh
git clone https://github.com/vajradevam/zen.git
cd zen
make
sudo make install
```

That installs, under `/usr/local`:

| File | What it is |
|---|---|
| `bin/zen` | the editor |
| `share/man/man1/zen.1` | the manual: `man zen` |
| `share/doc/zen/config.toml` | the default configuration, for reference |
| `share/doc/zen/README.md` | this document |
| `share/licenses/zen/LICENSE` | the licence |

Run `make` as yourself and only `make install` with `sudo`; installing never
builds, so it cannot leave root-owned files in the source directory.

**For your user only**, with no `sudo`:

```sh
make
make install PREFIX=~/.local
```

Make sure `~/.local/bin` is on your `PATH`.

**Somewhere else**: set `PREFIX` (default `/usr/local`). Packagers can also
set `DESTDIR`, for example `make install DESTDIR="$pkgdir" PREFIX=/usr`.

**With cargo alone**: `cargo install --git https://github.com/vajradevam/zen`
puts the binary in `~/.cargo/bin`. This skips the manual page, and the binary
is linked against the system C library instead of statically.

**Uninstall** with the same `PREFIX` you installed with:

```sh
sudo make uninstall
```

Your own configuration in `~/.config/zen` is never touched by installing or
uninstalling.

## Quick start

```sh
zen notes.txt          # edit, or start, a file
zen                    # start an empty text; saving asks for a name
git log | zen          # edit the output of a command
```

Then write. When you are done, type `;x` quickly to save and quit.

| Type | To |
|---|---|
| `;w` | save |
| `;x` | save and quit |
| `;q` | quit |
| `;Q` | quit without saving |
| `;u` | undo |
| `;f` | find |

## Commands

There are no Ctrl or Alt shortcuts. A command is a **sequence of ordinary
keys typed quickly**. The keys show up as text while you type them; when the
sequence completes they vanish and the command runs.

- Typed slower than `sequence_timeout` (500 ms by default), a sequence is
  just text. That is how you write a literal `;w`.
- A sequence is matched wherever it occurs, including inside a word. If you
  set `save = "evera"`, typing "several" quickly saves.
- Pasted text is never read as a command.

### Default sequences

| Sequence | Command | What it does |
|---|---|---|
| `;w` | `save` | save |
| `;W` | `save_as` | save under a new name |
| `;q` | `quit` | quit (refuses if there are unsaved changes) |
| `;Q` | `force_quit` | quit and throw away unsaved changes |
| `;x` | `save_quit` | save and quit |
| `;u` | `undo` | undo |
| `;r` | `redo` | redo |
| `;f` | `find` | find; an all-lowercase search ignores case |
| `;n` | `find_next` | next match |
| `;N` | `find_prev` | previous match |
| `;h` | `replace` | find and replace: `y` yes, `n` skip, `a` all, `q` stop |
| `;g` | `goto_line` | go to a line number |
| `;{` | `paragraph_up` | jump to the blank line above |
| `;}` | `paragraph_down` | jump to the blank line below |
| `;v` | `select` | start selecting, then move; again to stop |
| `;a` | `select_all` | select everything |
| `;c` | `copy` | copy the selection |
| `;d` | `cut` | cut the selection |
| `;p` | `paste` | paste |
| `;k` | `delete_line` | delete the current line |
| `;y` | `duplicate_line` | duplicate the current line |
| `;[` | `line_up` | move the current line up |
| `;]` | `line_down` | move the current line down |
| `;j` | `reflow` | re-wrap the current paragraph |
| `;#` | `word_count` | words and characters (of the selection, if any) |
| `;t` | `typewriter` | typewriter scrolling on / off |
| `;o` | `focus` | focus dimming on / off |
| `;;date` | text expansion | insert today's date |
| `;;time` | text expansion | insert the current time |

### Keys that are not sequences

| Key | Does |
|---|---|
| Arrows, Home, End, PageUp, PageDown | move |
| Ctrl+Left, Ctrl+Right | move by word |
| Ctrl+Home, Ctrl+End | start / end of the text |
| Shift + any movement | select |
| Backspace, Delete | delete a character, or the selection |
| Alt+Backspace, Ctrl+Delete | delete a word |
| Esc | drop the selection, or cancel a question |
| Mouse | click to place the cursor, drag to select, wheel to scroll |

## Configuration

`config.toml` in this repository is compiled into the binary as the defaults.
To change anything without rebuilding, make your own copy:

```sh
mkdir -p ~/.config/zen
zen --print-config > ~/.config/zen/config.toml
```

Then edit that file and restart zen.

- You only need to keep the lines you change; everything else falls back to
  its default.
- Every setting has a comment beside it explaining what it does.
- A mistake is reported with its line number when zen starts.
- `zen --config other.toml` uses a different file.

Font and font size are not settings: a terminal program cannot change them.
Set them in your terminal emulator.

### Switching features on and off

| Feature | Setting | Default | To switch |
|---|---|---|---|
| Typewriter scrolling | `typewriter` | off | `typewriter = true` |
| Focus dimming | `focus` | off | `focus = "paragraph"`, `"sentence"` or `"line"` |
| Mouse | `mouse` | on | `mouse = false` |
| Desktop clipboard | `system_clipboard` | on | `system_clipboard = false` |
| Crash recovery | `recovery` | on | `recovery = false` |
| Autosave | `autosave` | off | `autosave = true` |
| Changed-on-disk check | `check_disk` | on | `check_disk = false` |
| Remember position | `remember_position` | on | `remember_position = false` |
| Warnings on the bottom row | `messages` | on | `messages = false` |
| Auto-indent | `auto_indent` | on | `auto_indent = false` |
| Newline at end of file | `final_newline` | on | `final_newline = false` |
| Spaces instead of tabs | `soft_tabs` | off | `soft_tabs = true` |
| Centred text column | `center` | on | `center = false` |
| Any single command | its line under `[keys]` | on | `find = ""` |
| A text expansion | its line under `[expand]` | `;;date`, `;;time` | `";;date" = ""` |
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

### Editing

```toml
tab_width = 4
soft_tabs = false         # Tab inserts spaces instead of a tab character
auto_indent = true        # a new line keeps the indentation of the one above
final_newline = true      # make sure the file ends with a newline when saving
mouse = true              # false = leave the mouse to the terminal
```

With the mouse on, zen's selection replaces your terminal's own; most
terminals still give you theirs while you hold Shift.

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
- `xsel` occasionally loses very large copies on its own; prefer `xclip`, or
  `wl-clipboard` on Wayland, if you copy hundreds of kilobytes at a time.

If your setup needs something else, name the programs:

```toml
copy_command = "my-copy-tool"      # receives the text on its input
paste_command = "my-paste-tool"    # prints the clipboard
```

With `system_clipboard = false`, zen keeps a clipboard of its own.

### Safety

- **Crash recovery** (`recovery`): while there are unsaved changes, zen
  writes them every `recovery_interval` seconds to a hidden file beside
  yours. It is removed when you save or quit with `;Q`. If zen is killed or
  the terminal closes, the next start asks whether to bring the text back.
- **Autosave** (`autosave = true`): saves by itself once you have paused for
  `autosave_delay` seconds. Only for files that already have a name.
- **Changed-on-disk check** (`check_disk`): if another program changed the
  file since you opened it, or *save as* names a file that already exists,
  the save is refused with a warning. Save a second time to overwrite.
- **Remember position** (`remember_position`): a file reopens where you left
  it.

Saving always goes through a temporary file, so a save that fails half-way
cannot destroy the original.

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
exist); zen refuses to start and names the pair. The command names are in the
table under [Default sequences](#default-sequences).

```toml
sequence_timeout = 500    # ms allowed between the keys of a sequence; 0 = no limit
```

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

If the program fails, the text is left alone and its complaint is shown. A
program still running after 30 seconds is stopped.

## Files zen writes

| Path | What |
|---|---|
| `~/.config/zen/config.toml` | your configuration; zen only reads it |
| `~/.local/state/zen/positions` | where the cursor was in each file |
| `.NAME.zen-recover`, beside the file | unsaved text, removed on save |
| `~/.local/state/zen/unnamed.zen-recover` | the same, for a text with no name yet |

`$XDG_CONFIG_HOME` and `$XDG_STATE_HOME` are respected when set.

## Limits

- Linux only.
- Files must be UTF-8 text; anything else is refused rather than risked.
- Lines always wrap at word boundaries; there is no horizontal scrolling.
- One file at a time.

## Licence

zen is free software, released under the GNU General Public License,
version 2. See [LICENSE](LICENSE).
