//! tela: presentar con notas privadas superpuestas en Hyprland/Wayland.
//!
//! El binario `tela` presenta; `tela-gui` es el lanzador gráfico (GTK4) que prepara los
//! argumentos y se reemplaza a sí mismo por `tela`.

pub mod app;
pub mod args;
pub mod convert;
pub mod render;
pub mod settings;
