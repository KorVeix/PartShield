//! Pantallas de «Restaurar»: elegir la copia, elegir el disco, confirmar
//! con su número de serie y ver el progreso.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gio, glib};
use partshield_core::detect::devices::{human_size, Disk};

use crate::restore::{self, FoundSet, RestoreEvent, SetVerdict};
use crate::{error_status, group, header, model, plain_row};

fn verdict_look(v: SetVerdict) -> (&'static str, &'static str, &'static str) {
    match v {
        SetVerdict::Valid => ("emblem-ok-symbolic", "verdict-ok", "Comprobada"),
        SetVerdict::Reduced => (
            "dialog-warning-symbolic",
            "verdict-warn",
            "Sin comprobar del todo",
        ),
        SetVerdict::Invalid => ("dialog-error-symbolic", "verdict-bad", "No vale"),
    }
}

/// «27 nov 2026, 22:15» a partir de la fecha RFC 3339 del conjunto.
fn when(created: &str) -> String {
    const SHOWN: &str = "%d/%m/%Y %H:%M";
    chrono::DateTime::parse_from_rfc3339(created)
        .map(|d| d.format(SHOWN).to_string())
        .or_else(|_| {
            // Las copias sin conjunto solo guardan fecha y hora locales.
            chrono::NaiveDateTime::parse_from_str(created, "%Y-%m-%dT%H:%M:%S")
                .map(|d| d.format(SHOWN).to_string())
        })
        .unwrap_or_else(|_| created.to_string())
}

fn disk_name(set: &restore::BackupSet) -> String {
    match &set.disk.model {
        Some(m) => format!("{} · {}", m.trim(), human_size(set.disk.size)),
        None => format!("Disco de {}", human_size(set.disk.size)),
    }
}

// ------------------------------------------------------------ elegir copia

pub fn restore_page(app: &adw::Application, nav: &adw::NavigationView) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header(app));
    let page = adw::NavigationPage::builder()
        .title("Restaurar")
        .child(&toolbar)
        .build();
    let spinner = gtk::Spinner::new();
    spinner.start();
    spinner.set_size_request(32, 32);
    toolbar.set_content(Some(
        &adw::StatusPage::builder()
            .title("Buscando copias…")
            .description("En los discos conectados, sin escribir nada en ellos.")
            .child(&spinner)
            .build(),
    ));

    let (tb, app, nav) = (toolbar.clone(), app.clone(), nav.clone());
    glib::spawn_future_local(async move {
        let res = gio::spawn_blocking(|| {
            model::list_disks().map(|(disks, _)| {
                let found = restore::find_sets(&disks);
                (disks, found)
            })
        })
        .await;
        let w = match res {
            Ok(Ok((disks, found))) => sets_view(&app, &nav, Rc::new(disks), found),
            Ok(Err(e)) => error_status("No se pudieron leer los discos", &format!("{e:#}")),
            Err(_) => error_status("La búsqueda se interrumpió", "Error interno de PartShield."),
        };
        tb.set_content(Some(&w));
    });
    page
}

fn sets_view(
    app: &adw::Application,
    nav: &adw::NavigationView,
    disks: Rc<Vec<Disk>>,
    found: Vec<FoundSet>,
) -> gtk::Widget {
    if found.is_empty() {
        return adw::StatusPage::builder()
            .icon_name("drive-removable-media-symbolic")
            .title("No he encontrado copias")
            .description(
                "Conecta el disco donde las guardaste (carpeta partshield-copias) y vuelve a \
                 entrar en «Restaurar».\nLas copias hechas con versiones anteriores, o de \
                 particiones sueltas, se restauran desde la consola: partshield restore.",
            )
            .build()
            .upcast();
    }
    let page = adw::PreferencesPage::new();
    let list = group(
        "1 · ¿Qué copia restauro?",
        Some("La más reciente, primero."),
    );
    for f in found {
        let (icon, class, word) = verdict_look(f.set.verdict());
        let n = f.set.parts.len();
        let mut sub = format!(
            "{} · {n} {} · {word}",
            disk_name(&f.set),
            if n == 1 { "partición" } else { "particiones" }
        );
        let blocked = f.not_restorable();
        if let Some(why) = &blocked {
            sub.push_str(&format!(" · No se puede restaurar entera: {why}"));
        }
        let row = plain_row(&format!("Copia del {}", when(&f.set.created)), &sub);
        let img = gtk::Image::from_icon_name(icon);
        img.add_css_class(class);
        row.add_prefix(&img);
        if blocked.is_none() {
            row.set_activatable(true);
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let (app, nav, disks, f) = (app.clone(), nav.clone(), disks.clone(), Rc::new(f));
            row.connect_activated(move |_| nav.push(&target_page(&app, &nav, &disks, f.clone())));
        } else {
            row.add_css_class("dim-label");
        }
        list.add(&row);
    }
    page.add(&list);
    page.upcast()
}

// ------------------------------------------------------------ elegir disco

fn target_page(
    app: &adw::Application,
    nav: &adw::NavigationView,
    disks: &[Disk],
    found: Rc<FoundSet>,
) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header(app));
    let page = adw::PreferencesPage::new();
    let set = &found.set;

    let what = group(
        &format!("Copia del {}", when(&set.created)),
        Some(&disk_name(set)),
    );
    for p in &set.parts {
        let (icon, class, word) = verdict_look(p.verdict);
        let row = plain_row(&p.title, word);
        let img = gtk::Image::from_icon_name(icon);
        img.add_css_class(class);
        row.add_prefix(&img);
        what.add(&row);
    }
    page.add(&what);
    if set.verdict() != SetVerdict::Valid {
        let warn = adw::PreferencesGroup::new();
        let row = plain_row(
            "Esta copia no está comprobada del todo",
            "Alguna parte no pasó toda la comprobación. Se puede restaurar, pero no hay \
             garantía de que el resultado arranque o esté completo.",
        );
        let img = gtk::Image::from_icon_name("dialog-warning-symbolic");
        img.add_css_class("verdict-warn");
        row.add_prefix(&img);
        warn.add(&row);
        page.add(&warn);
    }

    let loops = std::env::var_os("PARTSHIELD_SANDBOX").is_some();
    let targets: Vec<Disk> = restore::restore_targets(disks, &found, loops)
        .into_iter()
        .cloned()
        .collect();
    let dest = group(
        "2 · ¿En qué disco?",
        Some(
            "Se borra el disco entero y se deja como estaba al hacer la copia. Solo salen \
             discos donde cabe; nunca el que guarda la copia ni el USB de PartShield.",
        ),
    );
    let anchor = gtk::CheckButton::new();
    let mut radios: Vec<(Disk, gtk::CheckButton)> = Vec::new();
    for d in &targets {
        let radio = gtk::CheckButton::new();
        radio.set_valign(gtk::Align::Center);
        radio.set_group(Some(&anchor));
        let original = d.serial.is_some() && d.serial == set.disk.serial;
        let mut sub = vec![d.path.display().to_string()];
        if let Some(s) = &d.serial {
            sub.push(format!("n.º de serie {s}"));
        }
        if original {
            sub.push("es el disco original".into());
        }
        let row = plain_row(&model::disk_title(d), &sub.join(" · "));
        row.add_prefix(&radio);
        row.set_activatable_widget(Some(&radio));
        dest.add(&row);
        radios.push((d.clone(), radio));
    }
    // Marcado al abrir: el disco original si está; si no, el único que haya.
    let pick = radios
        .iter()
        .position(|(d, _)| d.serial.is_some() && d.serial == set.disk.serial)
        .or((radios.len() == 1).then_some(0));
    if let Some((_, r)) = pick.and_then(|i| radios.get(i)) {
        r.set_active(true);
    }
    if targets.is_empty() {
        let row = plain_row(
            "No hay ningún disco donde quepa",
            &format!(
                "Hace falta un disco de al menos {} que no sea el de la copia.",
                human_size(found.min_size)
            ),
        );
        row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        dest.add(&row);
    }
    page.add(&dest);

    let go = gtk::Button::builder()
        .label("Restaurar…")
        .halign(gtk::Align::Center)
        .margin_top(12)
        .build();
    go.add_css_class("pill");
    go.add_css_class("destructive-action");
    let radios = Rc::new(radios);
    let refresh = {
        let (go, radios) = (go.clone(), radios.clone());
        move || go.set_sensitive(radios.iter().any(|(_, r)| r.is_active()))
    };
    refresh();
    for (_, r) in radios.iter() {
        let refresh = refresh.clone();
        r.connect_toggled(move |_| refresh());
    }
    let gg = adw::PreferencesGroup::new();
    gg.add(&go);
    page.add(&gg);

    {
        let (app, nav, found, radios) = (app.clone(), nav.clone(), found.clone(), radios.clone());
        go.connect_clicked(move |button| {
            let Some((disk, _)) = radios.iter().find(|(_, r)| r.is_active()) else {
                return;
            };
            confirm(button, &app, &nav, found.clone(), disk.clone());
        });
    }

    toolbar.set_content(Some(&page));
    adw::NavigationPage::builder()
        .title("Restaurar")
        .child(&toolbar)
        .build()
}

/// Pide teclear el final del número de serie del disco que se va a borrar.
fn confirm(
    parent: &gtk::Button,
    app: &adw::Application,
    nav: &adw::NavigationView,
    found: Rc<FoundSet>,
    disk: Disk,
) {
    let word = restore::confirmation_word(&disk);
    let mut body = format!(
        "Se va a borrar TODO lo que hay en:\n\n{}\n{}",
        model::disk_title(&disk),
        disk.path.display()
    );
    if let Some(s) = &disk.serial {
        body.push_str(&format!("\nN.º de serie: {s}"));
    }
    body.push_str(&if disk.serial.is_some() && word != "BORRAR" {
        format!("\n\nPara confirmar, escribe los 4 últimos caracteres del número de serie: {word}")
    } else {
        "\n\nEste disco no tiene número de serie. Para confirmar, escribe: BORRAR".to_string()
    });
    let dialog = adw::AlertDialog::new(
        Some("¿Borrar este disco y restaurar la copia?"),
        Some(&body),
    );
    let entry = gtk::Entry::builder()
        .activates_default(true)
        .placeholder_text(&word)
        .build();
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", "Cancelar"), ("go", "Borrar y restaurar")]);
    dialog.set_response_appearance("go", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("go"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("go", false);
    {
        let (dialog, word) = (dialog.clone(), word.clone());
        entry.connect_changed(move |e| {
            dialog.set_response_enabled("go", e.text().trim().eq_ignore_ascii_case(&word));
        });
    }
    let (app, nav) = (app.clone(), nav.clone());
    dialog.connect_response(None, move |_, response| {
        if response == "go" {
            nav.push(&progress_page(&app, &nav, found.clone(), disk.clone()));
        }
    });
    dialog.present(Some(parent));
}

// ---------------------------------------------------------------- progreso

fn progress_page(
    app: &adw::Application,
    nav: &adw::NavigationView,
    found: Rc<FoundSet>,
    disk: Disk,
) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header(app));
    let page = adw::NavigationPage::builder()
        .title("Restaurando")
        .child(&toolbar)
        .can_pop(false)
        .build();

    let bar = gtk::ProgressBar::builder().show_text(true).build();
    let step = gtk::Label::new(Some("Comprobando los archivos de la copia…"));
    step.add_css_class("step-title");
    step.set_wrap(true);
    let elapsed = gtk::Label::new(Some("0:00:00"));
    elapsed.add_css_class("numeric");
    elapsed.add_css_class("dim-label");
    let log = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .child(&log)
        .min_content_height(220)
        .build();
    scroll.add_css_class("card");
    let details = gtk::Expander::builder()
        .label("Ver detalles")
        .child(&scroll)
        .margin_top(12)
        .build();
    let col = gtk::Box::new(gtk::Orientation::Vertical, 8);
    col.append(&step);
    col.append(&bar);
    col.append(&elapsed);
    col.append(&details);
    let status = adw::StatusPage::builder()
        .icon_name("document-revert-symbolic")
        .title("Restaurando")
        .description("No apagues el equipo ni desconectes ningún disco.")
        .child(&adw::Clamp::builder().maximum_size(640).child(&col).build())
        .vexpand(true)
        .build();
    toolbar.set_content(Some(&status));

    let total = restore::restore_steps(&found);
    let (tx, rx) = mpsc::channel::<RestoreEvent>();
    let current = Rc::new(Cell::new(None::<usize>));
    let start = Instant::now();
    {
        let (tb, nav, page2) = (toolbar.clone(), nav.clone(), page.clone());
        let target = disk.path.clone();
        glib::timeout_add_local(Duration::from_millis(250), move || {
            let buf = log.buffer();
            let mut finished = None;
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    RestoreEvent::Step(i, title) => {
                        current.set(Some(i));
                        step.set_text(&format!("Paso {} de {total} · {title}", i + 1));
                    }
                    RestoreEvent::Command(c) => {
                        buf.insert(&mut buf.end_iter(), &format!("$ {c}\n"));
                        let adj = scroll.vadjustment();
                        adj.set_value(adj.upper());
                    }
                    RestoreEvent::Finished(r) => finished = Some(r),
                }
            }
            // Por pasos, no por bytes: una partición grande tarda en avanzar.
            let frac = match (&finished, current.get()) {
                (Some(Ok(_)), _) => 1.0,
                (_, Some(i)) => i as f64 / total as f64,
                _ => 0.0,
            };
            bar.set_fraction(frac);
            bar.set_text(Some(&format!("{}%", (frac * 100.0).round())));
            let secs = start.elapsed().as_secs();
            elapsed.set_text(&format!(
                "{}:{:02}:{:02}",
                secs / 3600,
                secs / 60 % 60,
                secs % 60
            ));
            match finished {
                Some(r) => {
                    tb.set_content(Some(&result_view(&nav, &target, r)));
                    page2.set_can_pop(true);
                    page2.set_title("Resultado");
                    glib::ControlFlow::Break
                }
                None => glib::ControlFlow::Continue,
            }
        });
    }
    let (found, target) = ((*found).clone(), disk.path.clone());
    gio::spawn_blocking(move || restore::run_restore(&found, &target, tx));
    page
}

fn result_view(
    nav: &adw::NavigationView,
    target: &std::path::Path,
    result: Result<Vec<String>, String>,
) -> gtk::Widget {
    let page = adw::PreferencesPage::new();
    let (icon, title, desc, css) = match &result {
        Ok(_) => (
            "emblem-ok-symbolic",
            "Restaurada",
            format!(
                "{} ha quedado como estaba al hacer la copia. Apaga, quita el USB de \
                 PartShield y arranca desde el disco.",
                target.display()
            ),
            "success",
        ),
        Err(e) => (
            "dialog-error-symbolic",
            "La restauración NO ha terminado",
            format!("{e}\n\nEl disco puede haber quedado a medias: no lo des por bueno."),
            "error",
        ),
    };
    let head = adw::StatusPage::builder()
        .icon_name(icon)
        .title(title)
        .description(glib::markup_escape_text(&desc).as_str())
        .build();
    head.add_css_class(css);
    head.add_css_class("compact");
    let hg = adw::PreferencesGroup::new();
    hg.add(&head);
    page.add(&hg);
    if let Ok(done) = &result {
        let g = group("Lo que se ha hecho", None);
        for line in done {
            let row = plain_row(line, "");
            let img = gtk::Image::from_icon_name("emblem-ok-symbolic");
            img.add_css_class("verdict-ok");
            row.add_prefix(&img);
            g.add(&row);
        }
        page.add(&g);
    }
    let back = gtk::Button::builder()
        .label("Volver al inicio")
        .halign(gtk::Align::Center)
        .build();
    back.add_css_class("pill");
    {
        let nav = nav.clone();
        back.connect_clicked(move |_| {
            nav.pop_to_tag("inicio");
        });
    }
    let g = adw::PreferencesGroup::new();
    g.add(&back);
    page.add(&g);
    page.upcast()
}
