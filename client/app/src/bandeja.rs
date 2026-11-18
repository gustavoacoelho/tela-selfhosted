//! Ícone na bandeja do sistema.
//!
//! Existe por causa dos atalhos globais: se dá para transmitir sem olhar para
//! o app, ele não precisa ocupar um botão na barra de tarefas enquanto alguém
//! joga. Fechar a janela passa a escondê-la aqui — o processo continua vivo,
//! porque é ele que escuta as teclas.
//!
//! # O ícone diz o que está acontecendo
//!
//! Um app que some da vista e continua capturando a tela é exatamente o tipo
//! de coisa que precisa de sinal permanente. Por isso o ícone ganha um ponto
//! vermelho enquanto há transmissão no ar, e não só uma dica de balão: um
//! aviso que exige passar o mouse por cima não avisa ninguém.

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use crate::registro;

/// Identificador do ícone, para reencontrá-lo e trocar o estado depois.
pub const ID: &str = "principal";

const ITEM_ABRIR: &str = "abrir";
const ITEM_PARAR: &str = "parar";
const ITEM_SAIR: &str = "sair";

/// O item "Parar transmissão", guardado para poder ser ligado e desligado.
///
/// O `TrayIcon` não devolve o menu depois de construído, então quem quiser
/// mexer num item precisa ter segurado a referência. Vai no estado do Tauri
/// em vez de num `static` porque é o mesmo lugar onde o resto do app já vive.
struct ItemParar(MenuItem<tauri::Wry>);

/// Ícone parado e ícone no ar, embutidos no executável.
fn icone(no_ar: bool) -> Image<'static> {
    if no_ar {
        tauri::include_image!("icons/bandeja-no-ar.png")
    } else {
        tauri::include_image!("icons/bandeja.png")
    }
}

/// Cria o ícone e o menu.
pub fn instala(app: &AppHandle) -> tauri::Result<()> {
    let abrir = MenuItem::with_id(app, ITEM_ABRIR, "Abrir", true, None::<&str>)?;
    // Começa desabilitado: não há o que parar antes de haver transmissão, e um
    // item clicável que não faz nada é pior que um item apagado.
    let parar = MenuItem::with_id(app, ITEM_PARAR, "Parar transmissão", false, None::<&str>)?;
    let sair = MenuItem::with_id(app, ITEM_SAIR, "Sair do tela'", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&abrir, &parar, &PredefinedMenuItem::separator(app)?, &sair],
    )?;

    app.manage(ItemParar(parar.clone()));

    TrayIconBuilder::with_id(ID)
        .icon(icone(false))
        .tooltip("tela' — parado")
        .menu(&menu)
        // Sem isto, o clique esquerdo abriria o menu e o evento de clique
        // nunca chegaria em `on_tray_icon_event`.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, evento| match evento.id().as_ref() {
            ITEM_ABRIR => mostra_janela(app),
            ITEM_PARAR => {
                // O menu roda na thread principal e o desmonte demora; fazer
                // aqui congelaria a janela pelo tempo todo.
                crate::encerra_em_segundo_plano(app.clone(), "parada pela bandeja");
            }
            ITEM_SAIR => {
                registro::linha("saindo pela bandeja");
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|icone, evento| {
            // Clique esquerdo simples abre a janela — é o que todo mundo tenta
            // primeiro, antes de descobrir que existe menu no botão direito.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = evento
            {
                mostra_janela(icone.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// Reflete o estado da transmissão no ícone, na dica e no menu.
///
/// `no_ar` é o que está sendo transmitido, para a dica dizer *o quê* — saber
/// que "algo" está indo não ajuda quem esqueceu o que escolheu.
pub fn atualiza(app: &AppHandle, no_ar: Option<&str>) {
    // A parte que mexe no ícone é EMPURRADA para a thread principal em vez de
    // ser chamada direto. As APIs de bandeja e menu do Windows só valem lá, e
    // chamá-las de fora bloqueia até a principal atender.
    //
    // Isso travava o app: `parar` e `transmitir` são comandos síncronos do
    // Tauri, que rodam NA thread principal; ambos esperam o supervisor da
    // transmissão terminar; e o supervisor, ao terminar, chamava isto aqui.
    // A principal ficava presa no `join`, o supervisor preso esperando a
    // principal, e a janela virava "Não está respondendo".
    //
    // `run_on_main_thread` enfileira e volta na hora, então ninguém espera
    // ninguém: o `join` termina, a principal volta ao laço de mensagens e
    // aplica a mudança.
    let no_ar = no_ar.map(str::to_string);
    let alvo = app.clone();
    if let Err(e) = app.run_on_main_thread(move || aplica(&alvo, no_ar.as_deref())) {
        registro::linha(&format!("bandeja: não consegui agendar a mudança: {e}"));
    }
}

/// A mudança em si. **Só na thread principal** — ver [`atualiza`].
fn aplica(app: &AppHandle, no_ar: Option<&str>) {
    let Some(icone_bandeja): Option<TrayIcon> = app.tray_by_id(ID) else {
        return;
    };
    // Os erros vão para o log em vez de serem engolidos: um ícone que não
    // muda é indistinguível de um app que não está transmitindo, e essa é
    // justamente a informação que a bandeja existe para dar.
    let mut falhas = Vec::new();
    if let Err(e) = icone_bandeja.set_icon(Some(icone(no_ar.is_some()))) {
        falhas.push(format!("ícone: {e}"));
    }
    if let Err(e) = icone_bandeja.set_tooltip(Some(match no_ar {
        Some(f) => format!("tela' — transmitindo: {f}"),
        None => "tela' — parado".to_string(),
    })) {
        falhas.push(format!("dica: {e}"));
    }

    // "Parar transmissão" só fica clicável quando há o que parar. Sem isto o
    // item nasceria desabilitado e ficaria assim para sempre.
    match app.try_state::<ItemParar>() {
        Some(item) => {
            if let Err(e) = item.0.set_enabled(no_ar.is_some()) {
                falhas.push(format!("item parar: {e}"));
            }
        }
        None => falhas.push("item parar não registrado".to_string()),
    }

    if falhas.is_empty() {
        registro::linha(&format!(
            "bandeja: {}",
            no_ar.map_or("parado".to_string(), |f| format!("no ar ({f})"))
        ));
    } else {
        registro::linha(&format!("bandeja não atualizou — {}", falhas.join("; ")));
    }
}

/// Traz a janela de volta.
fn mostra_janela(app: &AppHandle) {
    if let Some(j) = app.get_webview_window("main") {
        let _ = j.show();
        let _ = j.unminimize();
        let _ = j.set_focus();
    }
}
