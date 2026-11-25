use std::process::ExitCode;

use construct_agent::autostart;

const USAGE: &str = "usage: construct-agent <command>

commands:
  install-service    start the agent at every login and start it now
  ensure-service     install-service, unless it is installed for this binary already
  uninstall-service  stop the agent and remove it from login
  status             ask the running agent for its status
  backfill           send the prompts already collected to Construct (costs a model call each):
                     backfill [--days N] [--limit N] [--dry-run]
  run                run the agent in the foreground (the service runs this)";

fn main() -> ExitCode {
    let cmd = std::env::args().nth(1);
    let result = match cmd.as_deref() {
        Some("install-service") => install(),
        Some("ensure-service") => ensure(),
        Some("uninstall-service") => uninstall(),
        Some("status") => return status(),
        Some("backfill") => return backfill(&std::env::args().skip(2).collect::<Vec<_>>()),
        Some("run") => run_in_foreground(),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Run the agent. On a Mac the agent runs on a thread of its own and the main thread keeps the
/// system's run loop going, which is how the program is told what application is in front.
#[cfg(target_os = "macos")]
fn run_in_foreground() -> std::io::Result<()> {
    // The main thread's stack size, which the agent was written for.
    let worker = std::thread::Builder::new().stack_size(8 << 20).spawn(run)?;
    construct_agent::mac_signals::run_main_loop_until(|| worker.is_finished());
    worker
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("the agent stopped unexpectedly")))
}

#[cfg(not(target_os = "macos"))]
fn run_in_foreground() -> std::io::Result<()> {
    run()
}

/// The path of this binary, which the service file must be able to hold.
fn agent_exe() -> std::io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    if exe.to_str().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the path of the agent binary is not valid UTF-8; move it and try again",
        ));
    }
    Ok(exe)
}

fn install() -> std::io::Result<()> {
    autostart::current()?.install(&agent_exe()?)?;
    println!("installed and started {}", autostart::SERVICE_NAME);
    Ok(())
}

fn ensure() -> std::io::Result<()> {
    autostart::current()?.ensure(&agent_exe()?)?;
    println!("{} is registered", autostart::SERVICE_NAME);
    Ok(())
}

fn uninstall() -> std::io::Result<()> {
    autostart::current()?.uninstall()?;
    println!("removed {}", autostart::SERVICE_NAME);
    Ok(())
}

/// Bind the agent socket first (a second agent must fail here, before it touches the
/// database), serve it on a thread, start the database in the background so `status` never
/// waits for it, and stop the database on SIGTERM or SIGINT.
#[cfg(unix)]
// One sequence of start-up and shutdown steps; split it when a part grows its own logic.
#[allow(clippy::too_many_lines)]
fn run() -> std::io::Result<()> {
    use construct_agent::database::{self, Database, Health, State};
    use construct_agent::follow::Stats;
    use construct_agent::host;
    use tokio::signal::unix::{SignalKind, signal};

    let socket = construct_agent::ipc::socket_path()?;
    let paths = database::Paths::from_env()?;
    let host_paths = host::Paths::from_env()?;
    let (health, construct_health) = (Health::default(), Health::default());
    construct_health.wait(database::reason::WAITING_FOR_DATABASE);
    let connectors = Stats::default();
    let activity = construct_agent::activity::Feed::new();
    let model_state = construct_agent::model::ModelState::default();
    let settings = open_settings();
    let dashboard = open_dashboard(settings.clone());
    // Built before the socket server: a chat answer runs on this runtime, driven by the
    // thread of its connection.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let link = construct_agent::chat::Link::default();
    let manage = open_manage(settings.as_ref(), &link);
    let server = construct_agent::ipc::Server::bind(&socket)?
        .with_database(health.clone())
        .with_construct(construct_health.clone())
        .with_connectors(connectors.clone())
        .with_activity(activity.clone())
        .with_model(model_state.clone())
        .with_service(login_service());
    let phones = open_phones(&rt);
    let server = match &phones {
        Some(phones) => server.with_phones(phones.clone()),
        None => server,
    };
    let sync_dashboard = dashboard.clone();
    let mut sync_numbers = None;
    let (server, render_tool) = match dashboard {
        Some(dashboard) => {
            let numbers = construct_agent::dashboard_numbers::Numbers::new(
                activity.clone(),
                dashboard.clone(),
                rt.handle().clone(),
            );
            sync_numbers = Some(numbers.clone());
            let read = numbers.clone();
            let tool = construct_agent::chat::dashboard_view::tool(
                dashboard.clone(),
                std::sync::Arc::new(move |source| read.read_data(source)),
                construct_agent::chat::dashboard_view::Calendar::default(),
            );
            let server = server
                .with_numbers(numbers)
                .with_details(construct_agent::dashboard_details::Details::new(
                    activity.clone(),
                    dashboard.clone(),
                    settings.clone(),
                    rt.handle().clone(),
                ))
                .with_dashboard(dashboard);
            (server, Some(tool))
        }
        None => (server, None),
    };
    let (server, chat, setup) = with_chat(
        with_settings(server, settings.as_ref()),
        settings.as_ref(),
        rt.handle(),
        &link,
        manage.as_ref(),
        &activity,
        render_tool,
    );
    if let (Some(phones), Some(chat)) = (&phones, &chat) {
        let live = construct_agent::phone::live::Live {
            chat: chat.clone(),
            link: link.clone(),
            dashboard: sync_dashboard,
            numbers: sync_numbers,
        };
        let mut engine = construct_agent::phone::sync::Engine::new(
            std::sync::Arc::new(live),
            construct_agent::phone::sync::local_calendar(),
        );
        if let Some(manage) = &manage {
            let manage = manage.clone();
            engine = engine.with_erasing(std::sync::Arc::new(move || manage.erasing()));
        }
        rt.block_on(phones.set_sync(std::sync::Arc::new(engine)));
    }
    // The screen samples: the sampler runs on a thread of its own and takes none until the person
    // switches them on. Only a Mac gives the signals yet; any other computer reads as unsupported.
    let screen = settings.as_ref().and_then(|settings| {
        construct_agent::records::data_dir().ok().map(|dir| {
            construct_agent::screen::Screen::new(
                settings.clone(),
                &dir,
                construct_agent::screen::platform_signals(),
            )
        })
    });
    if let Some(screen) = &screen {
        screen.set_observer(activity.screen_observer());
    }
    let server = match &screen {
        Some(screen) => server.with_screen(screen.clone()),
        None => server,
    };
    let screen_alive = screen.as_ref().map(construct_agent::screen::Screen::spawn);
    // What `memory-erase` clears besides the host's copy; the follower is given the handle below.
    let follow_erase = construct_agent::records::data_dir().ok().map(|dir| {
        let names = construct_agent::follow::source_names(&connectors::Registry::builtin());
        construct_agent::follow::Erase::new(&dir, &names)
    });
    if let (Some(manage), Some(sessions)) = (&manage, &follow_erase) {
        manage.set_own_stores(construct_agent::erase::own_stores(
            sessions,
            chat.as_ref(),
            manage,
            &activity,
            setup.as_ref(),
            screen.as_ref(),
            rt.handle(),
        ));
    }
    if let (Some(manage), Some(phones)) = (&manage, &phones) {
        let phones = phones.clone();
        manage.set_phone_reset(std::sync::Arc::new(move || {
            phones
                .reset_phones()
                .map_err(|_| "the paired phones could not be told".to_owned())
        }));
    }
    eprintln!(
        "construct-agent {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        server.path().display()
    );
    let server = match &manage {
        Some(manage) => server.with_manage(manage.clone()),
        None => server,
    };
    let (served_tx, served_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = served_tx.send(server.serve());
    });

    // The log readers need no database: they scan on a thread of their own, so neither the
    // first scan nor a slow disk holds up `status`. The thread stops when `follow_alive` is
    // dropped, so it is kept until the agent stops (see the end of this function).
    let follow_alive = spawn_follower(
        &connectors,
        settings.as_ref(),
        manage.as_ref(),
        &activity,
        follow_erase,
    );

    let (mut term, mut int) = rt.block_on(async {
        Ok::<_, std::io::Error>((
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        ))
    })?;

    let result = rt.block_on(async {
        let stop_signal = async {
            tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = int.recv() => "SIGINT",
            }
        };
        tokio::pin!(stop_signal);
        // The start future lives only inside this block, so a signal drops it before
        // the leftover PostgreSQL is stopped.
        let started = {
            let start = Database::start(paths.clone());
            tokio::select! {
                res = start => Ok(res),
                sig = &mut stop_signal => Err(sig),
            }
        };
        let database = match started {
            Ok(Ok(db)) => {
                health.set(State::Ready);
                eprintln!("database ready on {}", db.paths().socket_dir.display());
                Some(db)
            }
            Ok(Err(e)) => {
                health.fail(e.reason());
                eprintln!("database failed: {e}");
                None
            }
            Err(sig) => {
                eprintln!("{sig} while the database was starting; stopping");
                database::stop_leftover(&paths).await;
                return Ok(());
            }
        };
        // The activity store is a copy of the records, made in a task of its own: the host does
        // not wait for it and a stop signal cancels it. Without it the agent still runs.
        let activity_task = if let Some(db) = database.as_ref() {
            start_activity(db, &activity).await
        } else {
            activity.off("the database is not running");
            None
        };
        // The Construct host needs the database; without one it does not start.
        let started = start_construct_until_stopped(
            database.as_ref(),
            &host_paths,
            (&health, &construct_health),
            settings.as_ref().map(|s| (s, &model_state)),
            &mut stop_signal,
        )
        .await;
        let mut host = match started {
            Ok(host) => host,
            Err(sig) => {
                eprintln!("{sig} while the Construct host was starting; stopped");
                stop_activity(activity_task).await;
                stop_all(None, database.as_ref()).await;
                return Ok(());
            }
        };
        if let Some(host) = &host {
            link.set(host.connection().clone());
        }
        let watch = watch_database(database.as_ref(), &health);
        let watch_host = watch_construct(host.as_mut(), &construct_health);
        let result = tokio::select! {
            sig = &mut stop_signal => {
                eprintln!("{sig}: stopping");
                Ok(())
            }
            res = served_rx => match res {
                Ok(r) => r,
                Err(_) => Err(std::io::Error::other("the socket server stopped")),
            },
            () = watch => Ok(()),
            () = watch_host => Ok(()),
        };
        stop_activity(activity_task).await;
        stop_all(host, database.as_ref()).await;
        result
    });
    // Only now may the follower stop: it is the last thing running besides the exit.
    drop(follow_alive);
    drop(screen_alive);
    result
}

/// What `status` reports as `service`. The platform's service handle is built once, here:
/// building it can start a program (macOS asks `id` for the user), and `status` must not. Where
/// it cannot be built (no home directory, `id` fails) the check answers `None`, so the reply
/// has no `service` and the app shows it as unknown.
#[cfg(unix)]
fn login_service() -> impl Fn() -> Option<bool> + Send + Sync + 'static {
    let login = construct_agent::autostart::current()
        .inspect_err(|e| eprintln!("construct-agent: start at login cannot be checked: {e}"))
        .ok();
    move || login.as_ref().map(|login| login.is_installed())
}

/// The settings file. A file that cannot be read must not stop the agent: then `settings-*`
/// answer an error and no log reader starts.
#[cfg(unix)]
fn open_settings() -> Option<construct_agent::settings::Settings> {
    use construct_agent::settings;

    match settings::config_dir().and_then(|dir| settings::Settings::open(&dir)) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("settings unavailable: {e}");
            None
        }
    }
}

/// The phone service: it listens only when a phone is paired or a pairing is open. When it cannot
/// start (the phones' file is unreadable, or the port of the paired phones is taken), the phone
/// commands answer an error and the rest of the agent runs.
#[cfg(unix)]
fn open_phones(rt: &tokio::runtime::Runtime) -> Option<construct_agent::phone::Service> {
    use construct_agent::phone::{DEFAULT_PORT, Service};

    let started = match construct_agent::settings::config_dir() {
        Ok(dir) => rt
            .block_on(Service::start(
                dir,
                ([0, 0, 0, 0], DEFAULT_PORT).into(),
                None,
            ))
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    match started {
        Ok(phones) => Some(phones),
        Err(e) => {
            eprintln!("phones unavailable: {e}");
            None
        }
    }
}

/// Writes the default activity dashboard into the workspace when there is none, writes the guide
/// files for the chat, reads the document and keeps reading it when it changes. A failure is only
/// logged: the dashboard is a document the app can do without, and the guide files are text the
/// dashboard can do without too.
#[cfg(unix)]
fn open_dashboard(
    settings: Option<construct_agent::settings::Settings>,
) -> Option<construct_agent::dashboard_document::Dashboard> {
    use construct_agent::{
        dashboard, dashboard_document::Dashboard, dashboard_guide, records, workspace::Workspace,
    };

    let opened = records::data_dir()
        .map_err(construct_agent::workspace::Error::from)
        .and_then(|dir| Workspace::open(&dir))
        .and_then(|workspace| {
            let screen = settings
                .as_ref()
                .is_some_and(|settings| settings.collects(construct_agent::screen::SOURCE));
            let written = dashboard::ensure_default_for(&workspace, screen)?;
            Ok((workspace, written))
        });
    match opened {
        Ok((workspace, written)) => {
            if written {
                eprintln!("the default activity dashboard written");
            }
            if let Err(e) = dashboard_guide::write(&workspace) {
                eprintln!("the dashboard guide files are not written: {e}");
            }
            let switch = settings.map(
                |settings| -> construct_agent::dashboard_document::ScreenSwitch {
                    std::sync::Arc::new(move || settings.collects(construct_agent::screen::SOURCE))
                },
            );
            let dashboard = Dashboard::open_with(std::sync::Arc::new(workspace), switch);
            dashboard.watch();
            Some(dashboard)
        }
        Err(e) => {
            eprintln!("the dashboard is not available: {e}");
            None
        }
    }
}

/// Serve the chat commands; without settings (the model comes from them) or without a data
/// directory they answer an error. The chat is returned too, for the person's erasure.
#[cfg(unix)]
fn with_chat(
    server: construct_agent::ipc::Server,
    settings: Option<&construct_agent::settings::Settings>,
    handle: &tokio::runtime::Handle,
    link: &construct_agent::chat::Link,
    manage: Option<&construct_agent::manage::Manager>,
    activity: &construct_agent::activity::Feed,
    render_tool: Option<std::sync::Arc<dyn construct_agent::chat::tool::ChatTool>>,
) -> (
    construct_agent::ipc::Server,
    Option<construct_agent::chat::Chat>,
    Option<construct_agent::dashboard_setup::SetupJob>,
) {
    use construct_agent::chat::{Chat, Deadlines};
    let Some(settings) = settings else {
        return (server, None, None);
    };
    let chat = construct_agent::records::data_dir()
        .map_err(|e| e.to_string())
        .and_then(|dir| {
            Chat::new(settings.clone(), &dir, handle.clone(), Deadlines::default())
                .map(|chat| {
                    chat.with_link(link.clone())
                        .with_recorder(activity.recorder())
                })
                .map(|chat| with_file_tools(chat, &dir, render_tool))
                .map(|chat| match manage {
                    Some(manage) => chat.with_observer(manage.observer()),
                    None => chat,
                })
                .map(|chat| (chat, dir))
                .map_err(|e| e.to_string())
        });
    match chat {
        Ok((chat, dir)) => {
            let (server, setup) = with_setup_job(
                server.with_chat(chat.clone()),
                &chat,
                &dir,
                settings,
                manage,
                activity,
            );
            (server, Some(chat), setup)
        }
        Err(e) => {
            eprintln!("chat unavailable: {e}");
            (server, None, None)
        }
    }
}

/// Serve the setup of the dashboard as a background job that runs on `chat`. Without the
/// workspace folder it answers an error.
#[cfg(unix)]
fn with_setup_job(
    server: construct_agent::ipc::Server,
    chat: &construct_agent::chat::Chat,
    dir: &std::path::Path,
    settings: &construct_agent::settings::Settings,
    manage: Option<&construct_agent::manage::Manager>,
    activity: &construct_agent::activity::Feed,
) -> (
    construct_agent::ipc::Server,
    Option<construct_agent::dashboard_setup::SetupJob>,
) {
    use construct_agent::dashboard_setup::SetupJob;
    let workspace = match construct_agent::workspace::Workspace::open(dir) {
        Ok(workspace) => std::sync::Arc::new(workspace),
        Err(e) => {
            eprintln!("the dashboard setup is not available: {e}");
            return (server, None);
        }
    };
    let job =
        SetupJob::open(settings.clone(), chat.clone(), workspace, dir).with_feed(activity.clone());
    let job = match manage {
        Some(manage) => job.with_facts(manage.clone()),
        None => job,
    };
    (server.with_setup(job.clone()), Some(job))
}

/// `chat` with the file tools over the workspace folder in `dir`, the dashboard tool when there is
/// one, and the sub-agent tool after them.
/// Without the folder the chat has
/// no tools, and the reason (never a path) is logged. A tool file that does not fit the tools stops
/// the start.
#[cfg(unix)]
fn with_file_tools(
    chat: construct_agent::chat::Chat,
    dir: &std::path::Path,
    render_tool: Option<std::sync::Arc<dyn construct_agent::chat::tool::ChatTool>>,
) -> construct_agent::chat::Chat {
    use construct_agent::chat::tool::{chat_list, setup_list};
    let workspace = match construct_agent::workspace::Workspace::open(dir) {
        Ok(workspace) => std::sync::Arc::new(workspace),
        Err(e) => {
            eprintln!("workspace unavailable: {e}");
            return chat;
        }
    };
    // The names, texts and order of the tools are in the tool file, read here once.
    let render = render_tool.is_some();
    let setup = setup_list(
        construct_agent::chat::tool::setup_tools(&workspace, render_tool.clone()),
        render,
    );
    let chat = match setup {
        Ok(tools) => chat.with_setup_tools(tools),
        Err(e) => tool_file_refused(&e),
    };
    match chat_list(
        construct_agent::chat::tool::standard_tools(&workspace, render_tool),
        render,
    ) {
        Ok(tools) => chat.with_tools(tools),
        Err(e) => tool_file_refused(&e),
    }
}

/// The agent does not start with a tool file that does not fit its tools: that is a broken build,
/// not a state a person can mend, and a chat with no tools would look like one that works.
#[cfg(unix)]
fn tool_file_refused(reason: &str) -> ! {
    eprintln!("the tool file cannot be used: {reason}");
    std::process::exit(1)
}

/// Serve the settings commands from `settings`; without them they answer an error.
#[cfg(unix)]
fn with_settings(
    server: construct_agent::ipc::Server,
    settings: Option<&construct_agent::settings::Settings>,
) -> construct_agent::ipc::Server {
    match settings {
        Some(s) => server.with_settings(s.clone()),
        None => server,
    }
}

/// The sender of prompts to Construct, started on a thread of its own. Without settings (the model
/// comes from them) or a data directory there is none, and `backfill` answers an error.
#[cfg(unix)]
fn open_manage(
    settings: Option<&construct_agent::settings::Settings>,
    link: &construct_agent::chat::Link,
) -> Option<construct_agent::manage::Manager> {
    let settings = settings?;
    let opened = construct_agent::records::data_dir().and_then(|dir| {
        construct_agent::manage::Manager::open(&dir, settings.clone(), link.clone())
    });
    match opened {
        Ok(manage) => {
            manage.spawn();
            Some(manage)
        }
        Err(e) => {
            eprintln!("sending prompts to construct unavailable: {e}");
            None
        }
    }
}

/// Starts the follower of the session files of every known tool, on the agent's data directory.
/// New user prompts go to `manage`.
#[cfg(unix)]
fn spawn_follower(
    stats: &construct_agent::follow::Stats,
    settings: Option<&construct_agent::settings::Settings>,
    manage: Option<&construct_agent::manage::Manager>,
    activity: &construct_agent::activity::Feed,
    erase: Option<construct_agent::follow::Erase>,
) -> std::sync::mpsc::Sender<()> {
    let tools = connectors::Registry::builtin();
    let Some(settings) = settings else {
        // Without the user's switches nothing is collected: no reader starts.
        for name in construct_agent::follow::source_names(&tools) {
            stats.not_started(name, "not started: settings unavailable");
        }
        return std::sync::mpsc::channel().0;
    };
    let paths = construct_agent::ipc::absolute_env_path("HOME")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set"))
        .and_then(|home| Ok((home, construct_agent::records::data_dir()?)));
    let snapshots = match &paths {
        Ok((_, data)) => construct_agent::follow::snapshots(data),
        // The follower tells the tools why; the sources it is given none of.
        Err(e) => {
            for name in construct_agent::follow::SNAPSHOT_SOURCES {
                stats.not_started(name, &format!("not started: {e}"));
            }
            Vec::new()
        }
    };
    let settings = settings.clone();
    let switch: construct_agent::follow::Switch =
        std::sync::Arc::new(move |tool| settings.collects(tool));
    construct_agent::follow::spawn_observed(
        tools,
        snapshots,
        paths,
        stats,
        Some(switch),
        manage.map(construct_agent::manage::Manager::observer),
        Some(activity.recorder()),
        erase,
    )
}

/// [`start_construct`] (no host without a database) until it ends, or until `stop_signal` fires (`Err` with the signal's
/// name) or PostgreSQL stops answering (no host). In both cases the host that was begun is
/// stopped, and awaited, before this returns, so the caller may stop the database at once.
#[cfg(unix)]
async fn start_construct_until_stopped<S>(
    db: Option<&construct_agent::database::Database>,
    paths: &construct_agent::host::Paths,
    (health, construct_health): (
        &construct_agent::database::Health,
        &construct_agent::database::Health,
    ),
    model: Option<(
        &construct_agent::settings::Settings,
        &construct_agent::model::ModelState,
    )>,
    stop_signal: &mut S,
) -> Result<Option<construct_agent::host::Host>, &'static str>
where
    S: std::future::Future<Output = &'static str> + Unpin,
{
    use construct_agent::database::{State, reason};

    let Some(db) = db else {
        construct_health.fail(reason::NO_DATABASE);
        return Ok(None);
    };
    construct_health.set(State::Starting);
    let abort = tokio_util::sync::CancellationToken::new();
    let start = start_construct(db, paths, construct_health, model, &abort);
    tokio::pin!(start);
    tokio::select! {
        host = &mut start => Ok(host),
        sig = stop_signal => {
            abort.cancel();
            stop_all(start.await, None).await;
            Err(sig)
        }
        // `watch_database` reports it; do not wait for the host's own timeout.
        () = watch_database(Some(db), health) => {
            abort.cancel();
            stop_all(start.await, None).await;
            construct_health.fail(reason::NO_DATABASE);
            Ok(None)
        }
    }
}

/// Create the graph storage database and start the Construct host on it, and record the
/// outcome in `health`; a host that fails to start leaves the agent running without it.
#[cfg(unix)]
async fn start_construct(
    db: &construct_agent::database::Database,
    paths: &construct_agent::host::Paths,
    health: &construct_agent::database::Health,
    model: Option<(
        &construct_agent::settings::Settings,
        &construct_agent::model::ModelState,
    )>,
    abort: &tokio_util::sync::CancellationToken,
) -> Option<construct_agent::host::Host> {
    use construct_agent::database::{State, reason};
    use construct_agent::host::{GRAPH_DBNAME, Host};

    if let Err(e) = db.ensure_database(GRAPH_DBNAME).await {
        health.fail(reason::GRAPH_DATABASE);
        eprintln!("construct failed: {e}");
        return None;
    }
    match Host::start(paths, db.paths(), model, abort).await {
        Ok(host) => {
            health.set(State::Ready);
            eprintln!("construct ready on 127.0.0.1:{}", host.connection().port);
            Some(host)
        }
        Err(e) => {
            health.fail(reason::GEAR_START);
            eprintln!("construct failed: {e:#}");
            None
        }
    }
}

/// Make the activity database, then open the store in a task of its own, build it from the
/// record files and keep it up to date with what `feed` is given. A failure is only logged and
/// shown in `status`: the store is a copy and the agent does not need it.
#[cfg(unix)]
async fn start_activity(
    db: &construct_agent::database::Database,
    feed: &construct_agent::activity::Feed,
) -> Option<tokio::task::JoinHandle<()>> {
    use construct_agent::activity::{DBNAME, Store};
    use construct_agent::records::{self, FileSink};

    if let Err(e) = db.ensure_database(DBNAME).await {
        eprintln!("activity store off: {e}");
        feed.off(&e.to_string());
        return None;
    }
    let socket_dir = db.paths().socket_dir.clone();
    let feed = feed.clone();
    Some(tokio::spawn(async move {
        let opened = async {
            let store = Store::open(&socket_dir).await.map_err(|e| e.to_string())?;
            let data_dir = records::data_dir().map_err(|e| e.to_string())?;
            Ok::<_, String>((store, FileSink::new(&data_dir)))
        };
        match opened.await {
            Ok((store, files)) => {
                if let Ok(workspace) = records::data_dir()
                    .map_err(construct_agent::workspace::Error::from)
                    .and_then(|dir| construct_agent::workspace::Workspace::open(&dir))
                {
                    feed.watch_findings(std::sync::Arc::new(workspace));
                }
                feed.run(&store, &files).await;
            }
            Err(e) => {
                eprintln!("activity store off: {e}");
                feed.off(&e);
            }
        }
    }))
}

/// Cancel the activity task when it still runs, before PostgreSQL is stopped. What the dropped
/// pool leaves open is closed by PostgreSQL's fast shutdown, which ends every session; a rebuild
/// that was cut off is rolled back.
#[cfg(unix)]
async fn stop_activity(task: Option<tokio::task::JoinHandle<()>>) {
    if let Some(task) = task {
        task.abort();
        if let Err(e) = task.await
            && e.is_panic()
        {
            eprintln!("activity store off: its task panicked");
        }
    }
}

/// Resolves never; marks the database failed when PostgreSQL stops answering. The agent
/// itself keeps running.
#[cfg(unix)]
async fn watch_database(
    database: Option<&construct_agent::database::Database>,
    health: &construct_agent::database::Health,
) {
    use construct_agent::database::reason;
    if let Some(db) = database {
        db.watch().await;
        health.fail(reason::STOPPED_ANSWERING);
        eprintln!("database failed: PostgreSQL stopped answering");
    }
    std::future::pending::<()>().await;
}

/// The same for a Construct host that stops on its own.
#[cfg(unix)]
async fn watch_construct(
    host: Option<&mut construct_agent::host::Host>,
    health: &construct_agent::database::Health,
) {
    use construct_agent::database::reason;
    if let Some(host) = host {
        let res = host.stopped().await;
        health.fail(reason::GEAR_STOPPED);
        match res {
            Ok(()) => eprintln!("construct failed: the host stopped"),
            Err(e) => eprintln!("construct failed: {e:#}"),
        }
    }
    std::future::pending::<()>().await;
}

/// Stop the Construct host first, since it uses the database, then the database.
#[cfg(unix)]
async fn stop_all(
    host: Option<construct_agent::host::Host>,
    database: Option<&construct_agent::database::Database>,
) {
    if let Some(host) = host
        && let Err(e) = host.stop().await
    {
        eprintln!("construct: {e:#}");
    }
    if let Some(db) = database
        && let Err(e) = db.stop().await
    {
        eprintln!("{e}");
    }
}

#[cfg(not(unix))]
fn run() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "the agent socket is not implemented on this platform yet",
    ))
}

/// Print the agent's status line and exit 0 when it reports `running`; print
/// `stopped` with the reason and exit 1 otherwise.
#[cfg(unix)]
fn status() -> ExitCode {
    let reply = construct_agent::ipc::socket_path()
        .and_then(|p| construct_agent::ipc::query_status(&p))
        .and_then(|line| running_reply(&line));
    match reply {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("{}", stopped(&e.to_string()));
            ExitCode::FAILURE
        }
    }
}

/// `backfill [--days N] [--limit N] [--dry-run]`: ask the running agent to send the prompts it has
/// already collected. A dry run only counts them. The answer line is printed; the exit code is 1
/// when it is an error.
#[cfg(unix)]
fn backfill(args: &[String]) -> ExitCode {
    let request = match backfill_request(args) {
        Ok(request) => request,
        Err(e) => {
            eprintln!("error: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    // Counting the history reads every record file, which takes a while on a big one.
    let reply = construct_agent::ipc::socket_path().and_then(|path| {
        construct_agent::ipc::request(
            &path,
            &format!("backfill {request}"),
            std::time::Duration::from_secs(300),
        )
    });
    match reply {
        Ok(line) => {
            println!("{line}");
            let failed = serde_json::from_str::<serde_json::Value>(&line)
                .map_or(true, |v| v.get("error").is_some());
            if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("error: the agent did not answer: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
fn backfill(_: &[String]) -> ExitCode {
    eprintln!("error: unsupported platform");
    ExitCode::FAILURE
}

/// The JSON of the `backfill` command from its command-line flags.
#[cfg(unix)]
fn backfill_request(args: &[String]) -> Result<String, String> {
    let mut request = serde_json::Map::new();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--dry-run" => {
                request.insert("dry_run".into(), true.into());
            }
            "--days" | "--limit" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{flag} needs a number"))?;
                let number: u32 = value
                    .parse()
                    .map_err(|_| format!("{flag} needs a whole number, not {value}"))?;
                request.insert(flag.trim_start_matches("--").into(), number.into());
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(serde_json::Value::Object(request).to_string())
}

#[cfg(not(unix))]
fn status() -> ExitCode {
    println!("{}", stopped("unsupported platform"));
    ExitCode::FAILURE
}

/// The reply line itself when it is JSON saying `"status":"running"`.
#[cfg(unix)]
fn running_reply(line: &str) -> std::io::Result<String> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad reply: {e}"))
    })?;
    if value["status"] == "running" {
        Ok(line.to_string())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the agent did not report running",
        ))
    }
}

fn stopped(reason: &str) -> String {
    serde_json::json!({ "status": "stopped", "reason": reason }).to_string()
}
