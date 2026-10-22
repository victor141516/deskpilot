# deskpilot

Ejecutable para que un agente lea y maneje aplicaciones de escritorio. Funciona como servidor MCP (stdio) o como línea de comandos de una sola ejecución con salida JSON. No tiene interfaz ni servicio en segundo plano: cada proceso vive lo que dura la sesión MCP o la llamada.

Hoy incluye el adaptador de Windows. El núcleo (motor, herramientas, MCP, CLI, política, worker elevado) no depende del sistema operativo; ver [docs/arquitectura.md](docs/arquitectura.md).

## Principio: lo menos invasivo primero

Leer:

1. `list_windows` / `list_apps` / `find_elements` / `get_tree`: accesibilidad, barato y sin tocar nada.
2. `screenshot` / `ocr`: solo cuando la aplicación no expone accesibilidad. Las imágenes son lo que más tokens cuesta.

Actuar (`act` escala solo y se detiene en el primer intento verificado):

| Paso | Estrategia | Quita el foco al usuario |
|---|---|---|
| 1 | Acción de accesibilidad (UIA: Invoke, SelectionItem, Toggle, ExpandCollapse, Value, RangeValue, ScrollItem, LegacyIAccessible) | No |
| 2 | Mensajes de ventana para controles Win32 clásicos (`BM_CLICK`, `WM_SETTEXT`, `TCM_SETCURFOCUS`) | No |
| 3 | Teclado real: foco por accesibilidad, Tab, flechas dentro del contenedor y Ctrl+Tab en pestañas, comprobando por accesibilidad tras cada tecla; luego Espacio/Intro/etc. | Sí, y lo devuelve al terminar |
| 4 | Ratón real en el centro del elemento, comprobando que no lo tapa otra ventana | Sí; devuelve foco y puntero |

Cada intento se verifica leyendo el estado después (seleccionado, marcado, desplegado, valor, foco). Si no hay estado que leer (`press`), se compara una huella de la interfaz (foco, ventanas, árbol). Una pulsación sin efecto observable no se repite con otra estrategia salvo `escalate_unverified: true`, para no ejecutar dos veces algo que sí ocurrió.

`max_strategy` y `min_strategy` acotan la escalera; `--max-strategy messages` impide quitar el foco al usuario en toda la sesión.

## Compilar

```powershell
cargo build --release
# target\release\deskpilot.exe
```

Requiere Rust 1.85 o superior y Windows 10/11.

## Uso como MCP

```powershell
claude mcp add deskpilot -- C:\ruta\deskpilot.exe mcp
```

Cualquier cliente MCP por stdio sirve: comando `deskpilot.exe`, argumentos `["mcp"]`. El servidor envía instrucciones de uso en `initialize`.

## Uso por línea de comandos

```powershell
deskpilot tools                                   # lista de herramientas
deskpilot tools --schema                          # con esquemas JSON
deskpilot call list_windows --text
deskpilot call get_tree -a window=Sound --text
deskpilot call act -a window=Sound -a name=Playback -a role=tab_item -a action=select --text
deskpilot call screenshot '{"window":{"process":"notepad"},"max_size":1024}'
deskpilot call get_tree -a window.process=Taskmgr --elevated --text
```

- Argumentos: JSON en línea, `@fichero.json`, `-` (stdin) y/o `-a ruta=valor` (las rutas con puntos anidan; el valor se interpreta como JSON si puede).
- Salida: JSON `{ ok, data, text, images }`; con `--text`, el texto compacto que vería el agente. Las imágenes se guardan en `%TEMP%\deskpilot` (o `--out-dir`).

## Herramientas

| Herramienta | Qué hace |
|---|---|
| `info` | Sistema, privilegios, worker elevado y política activa |
| `list_monitors`, `list_windows`, `list_apps` | Monitores, ventanas (incluidos diálogos con propietario) y aplicaciones |
| `get_tree`, `find_elements`, `get_element`, `get_focused`, `element_at`, `read_text` | Lectura por accesibilidad |
| `act` | `press`, `select`, `toggle`, `check`, `uncheck`, `expand`, `collapse`, `set_value`, `focus`, `scroll_into_view` con escalado |
| `send_keys`, `mouse` | Entrada real directa (invasiva) |
| `window` | `activate`, `minimize`, `maximize`, `restore`, `close`, `set_bounds` |
| `launch` | Abre ejecutable, documento, URI (`ms-settings:sound`), elemento del panel (`mmsys.cpl`) o app (`shell:AppsFolder\<AUMID>`) y espera su ventana |
| `wait_for` | Espera a que aparezca o desaparezca una ventana o elemento, o a un estado |
| `screenshot`, `ocr`, `pixel` | Pantalla, monitor, región, ventana (sus propios píxeles aunque esté tapada) o elemento |
| `clipboard_get`, `clipboard_set` | Texto e imagen |
| `elevate` | Arranca el worker de administrador |

Referencias:

- Ventana: `0x18F1358` (HWND).
- Elemento: `0x18F1358/2A.6E1058.4.80000001.6E1058.FFFFFFFC.1` (ventana + runtime id de UIA). Siguen valiendo entre llamadas mientras el elemento exista.
- Coordenadas: píxeles físicos del escritorio virtual. De una captura a la pantalla: `pantalla = origin + px_imagen / scale`.

## Administrador

Windows no deja que un proceso normal lea por accesibilidad ni mande entrada o mensajes a ventanas de procesos elevados; aparecen con la marca `admin` en `list_windows`. Cualquier herramienta acepta `"elevated": true`:

- MCP: la primera vez se muestra el aviso UAC. Si se acepta, un worker elevado (el mismo ejecutable, `deskpilot worker`) atiende todas las llamadas elevadas por una tubería con nombre privada hasta que termina la sesión.
- CLI: `--elevated` hace lo mismo para una sola llamada.

Si se intenta actuar sobre una ventana de administrador sin elevar, el error es `elevation_required`.

## Política y auditoría

`%APPDATA%\deskpilot\config.json` (opcional):

```json
{
  "deny_processes": ["PioneerGame"],
  "max_strategy": "keyboard",
  "audit": true
}
```

- `deny_processes`: procesos sobre los que nunca se actúa (lectura sí). Útil para juegos con antitrampas.
- Las opciones `--deny-process`, `--max-strategy`, `--dry-run`, `--no-audit` y `--config` aplican a la sesión y se pasan también al worker elevado.
- Las llamadas que cambian algo se registran en `%LOCALAPPDATA%\deskpilot\audit.jsonl`.
- `--dry-run` describe lo que haría sin hacerlo.

## Limitaciones conocidas

- `PrintWindow` no pinta algunos controles clásicos (por ejemplo, las cabeceras de pestañas del diálogo Sonido). Con `screen_pixels: true` se captura lo que se ve en pantalla.
- Ventanas minimizadas: no se pueden capturar; hay que restaurarlas antes (`window` → `restore`).
- Juegos, contenido protegido y apps sin accesibilidad: solo `screenshot`/`ocr` y `mouse`.
- OCR: usa los idiomas OCR instalados en Windows.
