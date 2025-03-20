use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use authfile::Entity;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::termion::event::{Event, Key};
use ratatui::text::Text;
use ratatui::widgets::{Block, Clear, List};
use ratatui::{Terminal, TerminalOptions, Viewport};
use russh::keys::{PublicKey, ssh_key::public::KeyData, ssh_key::rand_core::OsRng};
use russh::server::{Auth, Config, Handle, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, Pty};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tui_textarea::TextArea;
mod authfile;

type SshTerminal = Terminal<CrosstermBackend<TerminalHandle>>;

// wraps a type T as Arc<Mutex<T>> so that it can be locked
// in asynchronous coroutines
fn new_atomic<T>(object: T) -> Atomic<T> {
    Arc::new(Mutex::new(object))
}

type Atomic<T> = Arc<Mutex<T>>;

#[derive(Default)]
struct App {
    pub history: Vec<String>,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("failed to disconnect client with id {0}")]
    ClientDisconnectFailed(usize),
    #[error("russh error")]
    Russh(#[from] russh::Error),
    #[error("failed to read authorization file")]
    Authfile(#[from] authfile::Error),
}

pub struct Client {
    channel: ChannelId,
    handle: Handle,
    terminal: SshTerminal,
    textarea: TextArea<'static>,
    // entity: Arc<Entity>,
}

struct TerminalHandle {
    sender: UnboundedSender<Vec<u8>>,
    // The sink collects the data which is finally sent to sender.
    sink: Vec<u8>,
}

impl TerminalHandle {
    async fn start(handle: Handle, channel_id: ChannelId) -> Self {
        let (sender, mut receiver) = unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(data) = receiver.recv().await {
                let result = handle.data(channel_id, data.into()).await;
                if result.is_err() {
                    eprintln!("Failed to send data: {:?}", result);
                }
            }
        });
        Self {
            sender,
            sink: Vec::new(),
        }
    }
}

// The crossterm backend writes to the terminal handle.
impl std::io::Write for TerminalHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.sink.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let result = self.sender.send(self.sink.clone());
        if result.is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                result.unwrap_err(),
            ));
        }

        self.sink.clear();
        Ok(())
    }
}

#[derive(Clone)]
struct AppServer {
    keychain: Atomic<Vec<Arc<Entity>>>,
    key_data_pool: Atomic<HashSet<KeyData>>,
    key_data_to_user: Atomic<HashMap<KeyData, Arc<Entity>>>,
    key_data_to_id: Atomic<HashMap<KeyData, Vec<usize>>>,
    id_to_user: Atomic<HashMap<usize, Arc<Entity>>>,
    clients: Atomic<HashMap<usize, Client>>,

    id: usize,
    app: Atomic<App>,
}

impl AppServer {
    pub async fn run(&mut self) -> Result<(), anyhow::Error> {
        let mut methods = russh::MethodSet::empty();
        methods.push(russh::MethodKind::PublicKey);

        let config = Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
            auth_rejection_time: std::time::Duration::from_secs(3),
            auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
            methods,
            keys: vec![
                russh::keys::PrivateKey::random(&mut OsRng, russh::keys::Algorithm::Ed25519)
                    .unwrap(),
            ],
            ..Default::default()
        };
        self.run_on_address(Arc::new(config), ("0.0.0.0", 2222))
            .await?;
        Ok(())
    }

    async fn check_role_and_reload(&mut self) -> Result<(), Error> {
        if self.id_to_user.lock().await[&self.id].role() == authfile::Role::Admin {
            self.reload().await?;
        } else {
            // write a helpful message to the bottom most statusline
        }
        Ok(())
    }

    async fn reload(&mut self) -> Result<(), Error> {
        let new_keychain = authfile::read(Path::new("./authfile")).await?;

        // freeze all maps in the server state
        {
            let mut keychain = self.keychain.lock().await;
            let mut key_data_pool = self.key_data_pool.lock().await;
            let mut key_data_to_id = self.key_data_to_id.lock().await;
            let mut key_data_to_user = self.key_data_to_user.lock().await;
            let mut clients = self.clients.lock().await;
            let mut id_to_user = self.id_to_user.lock().await;

            // find all strays
            for stray in key_data_pool.difference(&new_keychain.key_pool) {
                let Some(ids) = key_data_to_id.get(stray) else {
                    continue;
                };

                // these IDs are now invalid
                for id in ids.iter() {
                    let client = &clients[id];
                    if let Err(()) = client.handle.close(client.channel).await {
                        return Err(Error::ClientDisconnectFailed(*id));
                    }
                    clients.remove(id);
                    id_to_user.remove(id);
                }
                key_data_to_id.remove(stray);
            }

            *key_data_to_user = new_keychain
                .entities
                .iter()
                .map(|e| (e.key_data(), e.clone()))
                .collect();
            *keychain = new_keychain.entities;
            *key_data_pool = new_keychain.key_pool;
        }
        log::info!("authfile synchronized to memory");
        Ok(())
    }

    async fn entity(&mut self) -> Arc<Entity> {
        self.id_to_user.lock().await[&self.id].clone()
    }

    async fn announce(&mut self) {
        let entity = self.entity().await;
        self.app.lock().await.history.push(format!(
            "{} with {:?} privileges has joined",
            entity.name(),
            entity.role()
        ));
    }

    async fn render(&mut self) {
        let clients = self.clients.clone();
        let history: Vec<String> = self
            .app
            .lock()
            .await
            .history
            .iter()
            .rev()
            .take(20)
            .rev()
            .cloned()
            .collect();
        tokio::spawn(async move {
            for (
                _,
                Client {
                    terminal, textarea, ..
                },
            ) in clients.lock().await.iter_mut()
            {
                terminal
                    .draw(|f| {
                        // clear the screen
                        let area = f.area();
                        f.render_widget(Clear, area);

                        let layout = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints(vec![Constraint::Fill(1), Constraint::Length(4)])
                            .split(f.area());
                        let style = Style::default().fg(Color::Green);

                        let paragraphs: Vec<_> = history
                            .iter()
                            .map(|message| Text::styled(message.to_string(), style))
                            .collect();

                        let paragraphs = List::new(paragraphs);
                        f.render_widget(paragraphs, layout[0]);
                        f.render_widget(&*textarea, layout[1]);
                    })
                    .unwrap();
            }
        });
    }
}

impl Server for AppServer {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        let s = self.clone();
        self.id += 1;
        s
    }
    fn handle_session_error(&mut self, _error: <Self::Handler as russh::server::Handler>::Error) {
        eprintln!("Session error: {:#?}", _error);
    }
}

impl Handler for AppServer {
    type Error = Error;

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        {
            // let entity = self.entity().await;
            let channel = channel.id();
            let handle = session.handle();
            let terminal_handle = TerminalHandle::start(handle.clone(), channel.clone()).await;

            let backend = CrosstermBackend::new(terminal_handle);

            // the correct viewport area will be set when the client request a pty
            let options = TerminalOptions {
                viewport: Viewport::Fixed(Rect::default()),
            };

            let terminal = Terminal::with_options(backend, options).unwrap();
            let title = {
                let entity = self.entity().await;
                let role = if entity.role() == authfile::Role::Admin {
                    "-[admin]"
                } else {
                    ""
                };
                format!("[{}]{}", entity.name(), role)
            };

            let mut textarea = TextArea::default();
            textarea.set_block(Block::bordered().title(title));

            let mut clients = self.clients.lock().await;
            clients.insert(
                self.id,
                Client {
                    textarea,
                    channel,
                    handle,
                    terminal,
                },
            );
        }
        self.announce().await;
        Ok(true)
    }

    async fn auth_publickey(&mut self, _: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        // Search for the key in our keychain
        if let Some(entity) = self.key_data_to_user.lock().await.get(key.key_data()) {
            // freeze everything, again
            let mut id_to_user = self.id_to_user.lock().await;
            let mut key_data_to_id = self.key_data_to_id.lock().await;

            id_to_user.insert(self.id, entity.clone());

            key_data_to_id
                .entry(key.key_data().clone())
                .or_default()
                .push(self.id);

            return Ok(Auth::Accept);
        }
        Ok(Auth::reject())
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Sending Ctrl+C ends the session and disconnects the client
        if data == [3] {
            return Err(russh::Error::Disconnect.into());
        }

        // Alt+Return
        if data == [27, 13] {
            let text = {
                let mut clients = self.clients.lock().await;
                let current_client = clients.get_mut(&self.id).unwrap();
                let text = current_client.textarea.lines().to_vec().join("\n");

                // HACK: Clear the message on send.
                // Select all, delete.
                current_client.textarea.select_all();
                current_client
                    .textarea
                    .input(ratatui::termion::event::Event::Key(
                        ratatui::termion::event::Key::Delete,
                    ));
                text
            };
            let name = self.entity().await.name().to_string();
            let message = format!("[{name}]: {text}");
            self.app.lock().await.history.push(message);
        }

        if !data.is_empty() {
            let mut iterator = data.iter().skip(1).map(|d| Ok(*d));
            match ratatui::termion::event::parse_event(data[0], &mut iterator) {
                // Press `Ctrl-r` to reload the authorization file
                Ok(Event::Key(Key::Ctrl('r'))) => {
                    self.check_role_and_reload().await?;
                }
                Ok(keycode) => {
                    self.clients
                        .lock()
                        .await
                        .get_mut(&self.id)
                        .unwrap()
                        .textarea
                        .input(keycode);
                }
                Err(e) => {
                    log::warn!("failed to parse keyboard input data: {:?}: {e}", data);
                }
            }
        }

        self.render().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _: &str,
        col_width: u32,
        row_height: u32,
        _: u32,
        _: u32,
        _: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let rect = Rect {
            x: 0,
            y: 0,
            width: col_width as u16,
            height: row_height as u16,
        };

        {
            let mut clients = self.clients.lock().await;
            let client = clients.get_mut(&self.id).unwrap();
            client.terminal.resize(rect).unwrap();

            session.channel_success(channel)?;
        }
        self.render().await;

        Ok(())
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        let id = self.id;
        let clients = self.clients.clone();
        tokio::spawn(async move {
            let mut clients = clients.lock().await;
            clients.remove(&id);
        });
    }
}

#[tokio::main]
async fn main() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Debug)
        .init();

    let keychain = authfile::read(Path::new("./authfile")).await.unwrap();
    let key_data_pool = new_atomic(keychain.key_pool);
    let key_data_to_id = new_atomic(HashMap::new());
    let id_to_user = new_atomic(HashMap::new());
    let clients = new_atomic(HashMap::new());
    let key_data_to_user = new_atomic(
        keychain
            .entities
            .iter()
            .map(|e| (e.key_data(), e.clone()))
            .collect(),
    );
    let keychain = new_atomic(keychain.entities);

    let mut sh = AppServer {
        app: new_atomic(App::default()),
        keychain,
        id_to_user,
        key_data_to_id,
        key_data_pool,
        key_data_to_user,
        clients,
        id: 0,
    };
    sh.run().await.unwrap();
}
