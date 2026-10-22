# deskpilot

An executable that lets an agent read and drive desktop applications. It runs as an MCP server (stdio) or as a one-shot command line with JSON output. No UI and no background service: each process lives for the MCP session or the single call.

The Windows adapter ships today. The core (engine, tools, MCP, CLI, policy, elevated worker) is OS-agnostic; see [docs/arquitectura.md](docs/arquitectura.md) (Spanish).

## Install: ask your agent

Copy this block and paste it to your agent (Claude Code, Codex, oh my pi, Cursor…) on Windows:

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

To update: close the sessions using it and repeat step 1. To uninstall: delete the folder and the server entry in the client's configuration.

## Principle: least invasive first

Reading:

1. `list_windows` / `list_apps` / `find_elements` / `get_tree`: accessibility, cheap, touches nothing.
2. `screenshot` / `ocr`: only when the application exposes no accessibility data. Images are the most expensive output in tokens.

Acting (`act` escalates by itself and stops at the first verified attempt):

| Step | Strategy | Takes focus from the user |
|---|---|---|
| 1 | Accessibility action (UIA: Invoke, SelectionItem, Toggle, ExpandCollapse, Value, RangeValue, ScrollItem, LegacyIAccessible) | No |
| 2 | Window messages for classic Win32 controls (`BM_CLICK`, `WM_SETTEXT`, `TCM_SETCURFOCUS`) | No |
| 3 | Real keyboard: accessibility focus, Tab, arrows inside the container and Ctrl+Tab for tabs, checking through accessibility after every key; then Space/Enter/etc. | Yes, and gives it back afterwards |
| 4 | Real mouse at the element's centre, after checking no other window covers it | Yes; gives back focus and pointer |

Every attempt is verified by reading the state afterwards (selected, checked, expanded, value, focus). When there is no state to read (`press`), a UI fingerprint is compared (focus, windows, tree). A press with no observable effect is not repeated with another strategy unless `escalate_unverified: true`, so nothing that did happen runs twice.

`max_strategy` and `min_strategy` bound the ladder; `--max-strategy messages` forbids taking focus from the user for the whole session.

## Build from source

```powershell
cargo install --path crates/deskpilot --locked   # → %USERPROFILE%\.cargo\bin\deskpilot.exe
```

Requires Rust 1.85+ and Windows 10/11. The C runtime is linked statically (`.cargo/config.toml`), so the `.exe` does not need the Visual C++ Redistributable. Installing outside the repo keeps rebuilds from clashing with a running server (Windows locks the `.exe` while it runs).

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
| `info` | OS, privileges, elevated worker and active policy |
| `list_monitors`, `list_windows`, `list_apps` | Monitors, windows (including owned dialogs) and applications |
| `get_tree`, `find_elements`, `get_element`, `get_focused`, `element_at`, `read_text` | Accessibility reads |
| `act` | `press`, `select`, `toggle`, `check`, `uncheck`, `expand`, `collapse`, `set_value`, `focus`, `scroll_into_view` with escalation |
| `send_keys`, `mouse` | Direct real input (invasive) |
| `window` | `activate`, `minimize`, `maximize`, `restore`, `close`, `set_bounds` |
| `launch` | Opens an executable, document, URI (`ms-settings:sound`), control panel item (`mmsys.cpl`) or app (`shell:AppsFolder\<AUMID>`) and waits for its window |
| `wait_for` | Waits for a window or element to appear or disappear, or for a state |
| `screenshot`, `ocr`, `pixel` | Screen, monitor, region, window (its own pixels even when covered) or element |
| `clipboard_get`, `clipboard_set` | Text and images |
| `elevate` | Starts the administrator worker |

References:

- Window: `0x18F1358` (HWND).
- Element: `0x18F1358/2A.6E1058.4.80000001.6E1058.FFFFFFFC.1` (window + UIA runtime id). Valid across calls while the element exists.
- Coordinates: physical pixels of the virtual desktop. From a screenshot to the screen: `screen = origin + image_px / scale`.

## Administrator

Windows does not let a normal process read accessibility data from, or send input or messages to, windows of elevated processes; they carry the `admin` flag in `list_windows`. Every tool accepts `"elevated": true`:

- MCP: the first time, the UAC prompt appears. Once accepted, an elevated worker (the same executable, `deskpilot worker`) serves every elevated call over a private named pipe until the session ends.
- CLI: `--elevated` does the same for a single call.

Acting on an administrator window without elevation fails with `elevation_required`.

## Policy and audit

`%APPDATA%\deskpilot\config.json` (optional):

```json
{
  "deny_processes": ["SomeGame"],
  "max_strategy": "keyboard",
  "audit": true
}
```

- `deny_processes`: processes that are never acted on (reading is still allowed). Useful for games with anti-cheat.
- `--deny-process`, `--max-strategy`, `--dry-run`, `--no-audit` and `--config` apply to the session and are passed on to the elevated worker.
- Calls that change something are logged to `%LOCALAPPDATA%\deskpilot\audit.jsonl`.
- `--dry-run` describes what would happen without doing it.

## Known limitations

- `PrintWindow` does not paint some classic controls (for example the tab headers of the Sound dialog). `screen_pixels: true` captures what is visible on screen instead.
- Minimized windows cannot be captured; restore them first (`window` → `restore`).
- Games, protected content and apps without accessibility: only `screenshot`/`ocr` and `mouse`.
- OCR uses the OCR languages installed in Windows.

## License

MIT
