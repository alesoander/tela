# tela

Presenta un PDF o PowerPoint en Google Meet / Zoom con **tus notas encima, semitransparentes,
visibles solo para ti**. Las notas cambian solas de página al cambiar de diapositiva.

Hecho en Rust para Arch Linux + **Hyprland** (Wayland). Sin Electron, sin GPU, sin bucle de
render: en reposo usa 0 % de CPU, el binario pesa ~1 MB y en 1080p ocupa ~55–75 MB de RAM
(casi todo son las páginas ya rasterizadas, listas para mostrarse al instante).

```
┌───────────────────────── tu monitor ─────────────────────────┐
│  ┌──────────────── ventana «tela — charla.pdf» ────────────┐  │
│  │  Diapositiva 3                ← esto es lo que COMPARTES │  │
│  │                                                         │  │
│  │  NOTA 3: recordar el dato…    ← capa superpuesta (no es │  │
│  └──────────────────────────────    parte de la ventana)  ─┘  │
│                                         [ 04:12  3/20  10:41 ] │ ← panel, también privado
└───────────────────────────────────────────────────────────────┘
```

## Cómo funciona (y por qué así)

En Linux no hay una forma universal de "ocultar una ventana de la captura" como en Windows.
`tela` lo resuelve con dos superficies distintas:

1. **Una ventana normal** con la presentación. Es la que eliges en Meet/Zoom al compartir
   **una ventana**.
2. **Una capa `wlr-layer-shell`** (capa *overlay*) con el PDF de notas, encima de la ventana,
   que no recibe clics ni foco. Como no forma parte de la ventana, **la captura de ventana de
   Hyprland no la incluye**.

> ⚠️ **Comparte la VENTANA, no la pantalla completa.** Si compartes el monitor entero las notas
> se verán. (La regla `noscreenshare` de Hyprland no sirve aquí: pinta un rectángulo negro en
> lugar de la ventana oculta, y la audiencia vería un recuadro negro sobre la diapositiva.)

## Instalación en Arch Linux

Dependencias:

```bash
sudo pacman -S --needed rust pkgconf poppler-glib cairo wayland libxkbcommon xdg-desktop-portal-hyprland
```

Para abrir `.ppt` / `.pptx` / `.odp` directamente (opcional):

```bash
sudo pacman -S libreoffice-fresh
```

Compilar e instalar como paquete:

```bash
cd packaging/arch && makepkg -si
```

O sin paquete:

```bash
cargo install --path .
```

## Uso

```bash
tela charla.pdf --notas charla-notas.pdf
```

- **Archivo de notas:** un PDF (o PPT) donde la **página N contiene las notas de la diapositiva N**.
  Si se llama `charla.notas.pdf`, `charla-notas.pdf` o `charla_notas.pdf` y está junto a la
  presentación, se detecta solo y no hace falta `--notas`.
- **PowerPoint:** `tela charla.pptx` convierte el archivo a PDF con LibreOffice **una sola vez**
  y lo guarda en `~/.cache/tela/`. Las siguientes veces abre al instante.
- **Desfase:** si las notas tienen una portada extra o van corridas, usa `--desfase 1` (o `-1`),
  o ajústalo en vivo con `[` / `]` o `Ctrl+←` / `Ctrl+→`.

Pasos para presentar:

1. Ejecuta `tela charla.pdf`. Se abre a pantalla completa en el monitor enfocado (o en
   `--monitor DP-1`; los nombres los ves con `hyprctl monitors`).
2. En Meet: **Presentar ahora → Una ventana → «tela — charla.pdf»**.
   En Zoom: **Compartir pantalla → elige la ventana de tela**.
3. Comprueba la vista previa de Meet/Zoom: ahí no deben aparecer las notas.
4. Avanza con el teclado o el presentador (clicker) mientras la ventana de tela tenga el foco.

### Opciones

| Opción | Qué hace |
|---|---|
| `-n, --notas <archivo>` | PDF/PPT de notas |
| `-d, --desfase <N>` | Desfase notas ↔ diapositivas |
| `-o, --opacidad <v>` | Opacidad inicial de las notas: `0.6`, `60` o `60%` (por defecto 60 %) |
| `--tinta-clara` | Texto de las notas en claro, para diapositivas oscuras |
| `--sin-tinta` | Mostrar la página de notas con su fondo blanco |
| `-p, --pagina <N>` | Empezar en la diapositiva N |
| `-m, --monitor <nombre>` | Monitor donde presentar |
| `-w, --ventana` | Empezar en ventana (no pantalla completa) |

### Teclas

| Tecla | Acción |
|---|---|
| `→` `↓` `AvPág` `Espacio` `Intro` · clic izquierdo · rueda | Siguiente |
| `←` `↑` `RePág` `Retroceso` · clic derecho · rueda | Anterior |
| `Inicio` / `Fin` | Primera / última |
| número + `Intro` | Ir a esa diapositiva (`Esc` cancela) |
| `H` | Mostrar / ocultar notas |
| `+` / `-` | Opacidad de las notas |
| `I` | Tinta oscura → tinta clara → página completa |
| `[` `]` o `Ctrl+←` `Ctrl+→` | Desfase de notas −1 / +1 |
| `T` / `R` | Cronómetro: iniciar-pausar / poner a cero |
| `C` | Mostrar / ocultar el panel del cronómetro |
| `B` o `.` | Pantalla negra para la audiencia (las notas siguen visibles para ti) |
| `F` / `F11` | Pantalla completa sí/no (`F5` activa, `Esc` sale) |
| `Q` | Salir |

Los presentadores USB normales envían `AvPág`/`RePág`, `F5`/`Esc` y `B` o `.`, así que funcionan
sin configurar nada.

**Modo tinta:** el blanco del PDF de notas se vuelve transparente y solo queda el texto (con sus
colores). En diapositivas oscuras usa **tinta clara** (`I`), que aclara el texto sin cambiar
su tono.

**Cronómetro:** arranca solo la primera vez que avanzas de diapositiva. El panel muestra tiempo
transcurrido, diapositiva actual/total y la hora.

## Ajustes opcionales de Hyprland

Para que las notas aparezcan sin animación de entrada:

```ini
layerrule = noanim, tela-notas
layerrule = noanim, tela-panel
```

(En las versiones de Hyprland con la nueva sintaxis de reglas, adapta estas líneas según la wiki
de tu versión.)

## Notas y limitaciones

- Las teclas llegan a tela solo si su ventana tiene el foco. Si haces clic en Meet en el otro
  monitor, vuelve a enfocar la presentación antes de usar el clicker.
- En pantalla completa el cursor se oculta sobre la presentación para que no salga en lo
  compartido.
- Las notas cubren todo el monitor de la presentación. Coinciden con la diapositiva si ambos
  PDF tienen la misma proporción (p. ej. 16:9).
- Zoom: si no te ofrece compartir ventanas en Wayland, revisa que use el portal de escritorio
  (`enableWaylandShare=true` en `~/.config/zoomus.conf`) o usa la versión web.

## Arquitectura

| Archivo | Qué hace |
|---|---|
| `src/main.rs` | Arranque y errores |
| `src/args.rs` | Línea de comandos (sin dependencias) |
| `src/convert.rs` | PPT/PPTX/ODP → PDF con LibreOffice headless, con caché y perfil propio |
| `src/render.rs` | Hilo de render con poppler + cairo; cola de prioridad (actual → siguiente), modos de tinta |
| `src/app.rs` | Cliente Wayland (smithay-client-toolkit): ventana xdg, capas overlay, teclado, ratón, panel |

Detalles que reducen el consumo:

- **No hay bucle de render.** Solo se dibuja cuando algo cambia; el panel se actualiza como mucho
  una vez por segundo.
- **Cero copias al cambiar de página.** Las páginas se rasterizan en un hilo aparte directamente
  al tamaño físico del monitor (con escala fraccional) y quedan en búferes `wl_shm` compartidos
  con el compositor. Avanzar es adjuntar un búfer ya hecho; la siguiente página se precarga.
- **Opacidad por el compositor.** Con `wp_alpha_modifier_v1` la opacidad la aplica Hyprland sin
  coste de CPU. Si el compositor no lo soporta, se hornea al rasterizar.
- **Búferes de 1×1 px** para la pantalla negra y las notas vacías (los estira `wp_viewporter`).
- Al ocultar las notas (`H`) se destruye su capa y se libera su memoria.
- En pantallas grandes (4K) solo se guardan dos páginas por superficie.

Para ver los tiempos de render y las teclas recibidas: `TELA_DEBUG=1 tela charla.pdf`.
