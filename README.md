# deskpilot

An executable that lets an agent read and drive desktop applications. It runs as an MCP server (stdio) or as a one-shot command line with JSON output. No background service: each process lives for the MCP session or the single call. Its only UI is the macOS permission window, shown only while a permission is missing.

Adapters ship for Windows and macOS. The core (engine, tools, MCP, CLI, policy, elevated worker) is OS-agnostic; see [docs/architecture.md](docs/architecture.md).

## Install: ask your agent

### Windows

Copy this block and paste it to your agent (Claude Code, Codex, oh my pi, Cursor…):

````text
Install the "deskpilot" MCP server (Windows desktop automation, https://github.com/victor141516/deskpilot) and register it in this MCP client for my user.

1. Download the latest release executable to %LOCALAPPDATA%\Programs\deskpilot\deskpilot.exe with PowerShell:
   $dir = "$env:LOCALAPPDATA\Programs\deskpilot"
   New-Item -ItemType Directory -Force $dir | Out-Null
   Invoke-WebRequest -Uri "https://github.com/victor141516/deskpilot/releases/latest/download/deskpilot.exe" -OutFile "$dir\deskpilot.exe"
   Unblock-File "$dir\deskpilot.exe"
2. Check that it runs: & "$dir\deskpilot.exe" --version  (it must print "deskpilot <version>").
3. Register it as a stdio MCP server named "deskpilot", command = absolute path of the .exe (no environment variables), arguments ["mcp"]. If the client supports a per-server or per-tool timeout, set it to 180 seconds: the "elevate" tool waits for the user to accept the UAC prompt (up to 120 s).
   - Claude Code: claude mcp add --scope user deskpilot -- "<path>\deskpilot.exe" mcp
   - Codex: in ~/.codex/config.toml add
       [mcp_servers.deskpilot]
       command = '<path>\deskpilot.exe'
       args = ["mcp"]
       tool_timeout_sec = 180
   - oh my pi: in ~/.omp/agent/mcp.json, inside "mcpServers", add
       "deskpilot": { "type": "stdio", "command": "<path with forward slashes>/deskpilot.exe", "args": ["mcp"], "timeout": 180000 }
   - Any other client: the equivalent entry in its MCP configuration file, without removing existing servers.
4. Reload the MCP servers (or tell me to restart the client) and call deskpilot's "info" tool to confirm it answers. Tell me the installed path and the configuration file you changed.
````

### macOS

Copy this block and paste it to your agent on macOS:

````text
Install the "deskpilot" MCP server (macOS desktop automation, https://github.com/victor141516/deskpilot) and register it in this MCP client for my user.

1. Download the latest universal release binary to ~/.local/bin/deskpilot:
   mkdir -p ~/.local/bin
   curl -fsSL "https://github.com/victor141516/deskpilot/releases/latest/download/deskpilot-macos-universal" -o ~/.local/bin/deskpilot
   chmod +x ~/.local/bin/deskpilot
   codesign -s - -f --identifier dev.deskpilot ~/.local/bin/deskpilot
2. Check that it runs: ~/.local/bin/deskpilot --version  (it must print "deskpilot <version>").
3. Register it as a stdio MCP server named "deskpilot", command = absolute path of the binary (no environment variables), arguments ["mcp"]. If the client supports a per-server timeout, set it to 180 seconds.
   - Claude Code: claude mcp add --scope user deskpilot -- ~/.local/bin/deskpilot mcp
   - Codex: in ~/.codex/config.toml add
       [mcp_servers.deskpilot]
       command = '<absolute path>/.local/bin/deskpilot'
       args = ["mcp"]
       tool_timeout_sec = 180
   - oh my pi: in ~/.omp/agent/mcp.json, inside "mcpServers", add
       "deskpilot": { "type": "stdio", "command": "<absolute path>/.local/bin/deskpilot", "args": ["mcp"], "timeout": 180000 }
   - Any other client: the equivalent entry in its MCP configuration file, without removing existing servers.
4. Reload the MCP servers (or tell me to restart the client) and call deskpilot's "info" tool to confirm it answers. If the permission window appears, grant Accessibility and Screen Recording permissions in System Settings. Tell me the installed path and the configuration file you changed.
````

To update: close the sessions using it and repeat step 1. To uninstall: delete the binary and the server entry in the client's configuration.
## Principle: least invasive first

Reading:

1. `list_windows` / `list_apps` / `find_elements` / `get_tree`: accessibility, cheap, touches nothing.
2. `screenshot` / `ocr`: only when the application exposes no accessibility data. Images are the most expensive output in tokens.

Acting (`act` escalates by itself and stops at the first verified attempt):

| Step | Strategy | Takes focus from the user |
|---|---|---|
| 1 | Accessibility action (Windows UIA: Invoke, SelectionItem, Toggle, ExpandCollapse, Value, RangeValue, ScrollItem, LegacyIAccessible; macOS AX: AXPress, AXPick, AXSelected, AXExpanded, AXValue, AXScrollToVisible) | No |
| 2 | Window messages for classic Win32 controls (`BM_CLICK`, `WM_SETTEXT`, `TCM_SETCURFOCUS`); Windows only | No |
| 3 | Real keyboard: accessibility focus, Tab, arrows inside the container and the next-tab chord (Ctrl+Tab) for tabs, checking through accessibility after every key; then Space/Enter/etc. | Yes, and gives it back afterwards |
| 4 | Real mouse at the element's centre, after checking no other window covers it | Yes; gives back focus and pointer |

Every attempt is verified by reading the state afterwards (selected, checked, expanded, value, focus). When there is no state to read (`press`), a UI fingerprint is compared (focus, windows, tree). A press with no observable effect is not repeated with another strategy unless `escalate_unverified: true`, so nothing that did happen runs twice.

`max_strategy` and `min_strategy` bound the ladder; `--max-strategy messages` forbids taking focus from the user for the whole session.

## Build from source

```sh
cargo install --path crates/deskpilot --locked   # → ~/.cargo/bin/deskpilot (deskpilot.exe on Windows)
```

Requires Rust 1.85+ and Windows 10/11 or macOS 14+.

- Windows: the C runtime is linked statically (`.cargo/config.toml`), so the `.exe` does not need the Visual C++ Redistributable. Installing outside the repo keeps rebuilds from clashing with a running server (Windows locks the `.exe` while it runs).
- macOS: macOS ties privacy permissions to the binary's code signature, and every build gets a new ad-hoc one, so a rebuilt binary asks again. Sign with a stable identity to keep them: `codesign -s "<your Developer ID or Apple Development identity>" -f ~/.cargo/bin/deskpilot`.

The server sends its usage instructions in `initialize`, and every tool has a description and schema, so agents need no separate documentation.

## Command line

```powershell
deskpilot tools                                   # list tools
deskpilot tools --schema                          # with JSON schemas
deskpilot call list_windows --text
deskpilot call get_tree -a window=Sound --text
deskpilot call act -a window=Sound -a name=Playback -a role=tab_item -a action=select --text
deskpilot call screenshot '{"window":{"process":"notepad"},"max_size":1024}'
deskpilot call get_tree -a window.process=Taskmgr --elevated --text
```

- Arguments: inline JSON, `@file.json`, `-` (stdin) and/or `-a path=value` (dotted paths nest; values are parsed as JSON when possible).
- Output: JSON `{ ok, data, text, images }`; with `--text`, the compact text an agent would see. Images are saved to `%TEMP%\deskpilot` (or `--out-dir`).

## Tools

| Tool | What it does |
|---|---|
| `info` | OS, privileges, elevated worker (Windows), privacy permissions (macOS) and active policy |
| `list_monitors`, `list_windows`, `list_apps` | Monitors, windows (including owned dialogs) and applications |
| `get_tree`, `find_elements`, `get_element`, `get_focused`, `element_at`, `read_text` | Accessibility reads |
| `act` | `press`, `select`, `toggle`, `check`, `uncheck`, `expand`, `collapse`, `set_value`, `focus`, `scroll_into_view` with escalation |
| `send_keys`, `mouse` | Direct real input (invasive) |
| `window` | `activate`, `minimize`, `maximize`, `restore`, `close`, `set_bounds` |
| `launch` | Opens an executable, document, folder, URI or app and waits for its window. Windows: `ms-settings:sound`, `mmsys.cpl`, `shell:AppsFolder\<AUMID>`. macOS: app name (`TextEdit`), bundle id, `.app` path, `x-apple.systempreferences:` URI |
| `wait_for` | Waits for a window or element to appear or disappear, or for a state |
| `screenshot`, `ocr`, `pixel` | Screen, monitor, region, window (its own pixels even when covered) or element |
| `clipboard_get`, `clipboard_set` | Text and images |
| `elevate` | Starts the administrator worker (Windows only) |

References:

- Window: `0x18F1358` (HWND) on Windows, `20763` (CGWindowID) on macOS.
- Element: `0x18F1358/2A.6E1058.4.80000001.6E1058.FFFFFFFC.1` (window + UIA runtime id) on Windows; `20763/0.3.1~9f2c41d0` (window + child path + fingerprint) on macOS. Valid across calls while the element exists.
- Coordinates: physical pixels of the virtual desktop on Windows, points on macOS. From a screenshot to the screen: `screen = origin + image_px / scale` (`scale` already includes the Retina factor).

## Administrator (Windows)

Windows does not let a normal process read accessibility data from, or send input or messages to, windows of elevated processes; they carry the `admin` flag in `list_windows`. Every tool accepts `"elevated": true`:

- MCP: the first time, the UAC prompt appears. Once accepted, an elevated worker (the same executable, `deskpilot worker`) serves every elevated call over a private named pipe until the session ends.
- CLI: `--elevated` does the same for a single call.

Acting on an administrator window without elevation fails with `elevation_required`.

## Permissions (macOS)

macOS asks the user before an app can read other apps' controls and send input (**Accessibility**) or capture the screen (**Screen & System Audio Recording**). Administrator rights do not help, so on macOS there is no `elevate` tool and no `elevated` argument.

- On start-up deskpilot checks both. If one is missing it shows a small window listing them, each with *Request* (the system prompt) and *Open Settings* (the exact Privacy & Security pane). The window checks every second, turns each row green when granted and closes by itself once everything is granted. It is not shown while both are granted, so it only comes back if a permission is revoked.
- `deskpilot call …` waits for the window. `deskpilot mcp` does not (the client would time out): the window stays up next to the session, and tools that need a missing permission fail with `permission_required`.
- Permissions are granted to deskpilot itself, not to the terminal or MCP client that launches it.
- macOS applies a new Screen Recording grant to new processes only: a waiting `call` restarts itself; an MCP session reports `permission_required` until the server is restarted.
- `info` reports both permissions.

## Policy and audit

`%APPDATA%\deskpilot\config.json` on Windows, `~/Library/Application Support/deskpilot/config.json` on macOS (optional):

```json
{
  "deny_processes": ["SomeGame"],
  "max_strategy": "keyboard",
  "audit": true
}
```

- `deny_processes`: processes that are never acted on (reading is still allowed). Useful for games with anti-cheat.
- `--deny-process`, `--max-strategy`, `--dry-run`, `--no-audit` and `--config` apply to the session and are passed on to the elevated worker.
- Calls that change something are logged to `%LOCALAPPDATA%\deskpilot\audit.jsonl` (macOS: `~/Library/Application Support/deskpilot/audit.jsonl`).
- `--dry-run` describes what would happen without doing it.

## Known limitations

- `PrintWindow` does not paint some classic controls (for example the tab headers of the Sound dialog). `screen_pixels: true` captures what is visible on screen instead.
- Minimized windows cannot be captured; restore them first (`window` → `restore`).
- Games, protected content and apps without accessibility: only `screenshot`/`ocr` and `mouse`.
- OCR uses the OS engine: the OCR languages installed in Windows, Vision's languages on macOS.
- macOS: Tab reaches every control only with *Keyboard navigation* enabled (System Settings → Keyboard); otherwise the keyboard step reaches only text fields and lists and `act` falls back to the mouse. Pop-up buttons have no universal opening key, so `expand` relies on accessibility or the mouse.
- macOS: long tables and lists expose only their visible rows in `get_tree`/`find_elements`; scroll to reach the rest.
- macOS: some apps draw toolbar buttons with an icon and a separate text label (Fork's Fetch/Pull/Push): the button itself has no name. Use `element_at` on the label's position, or `ocr` + `mouse`.
- macOS: browsers and Electron apps expose their page content to deskpilot (it enables their accessibility on first use), but a page that is still loading or rendered in a canvas has nothing to read; use `screenshot`/`ocr`.
- macOS: mapping windows to accessibility uses the private `_AXUIElementGetWindow`, as window managers do.

## License

MIT
