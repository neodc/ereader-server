use std::io::Error as IoError;
use std::sync::{Mutex, Arc};
use std::env;
use tokio::net::{TcpListener, TcpStream};
use std::net::SocketAddr;
use tokio_tungstenite::tungstenite::{Message, Result as TungsteniteResult};
use futures_util::StreamExt;
use futures_util::SinkExt;
use tokio::sync::broadcast::Sender;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::channel;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender, UnboundedReceiver};
use serde::Serialize;
use serde::Deserialize;
use futures_util::stream::SplitSink;
use tokio_tungstenite::WebSocketStream;
use mpd_client::{commands, Client, Subsystem};
use mpd_client::commands::responses::{PlayState, self};
use mpd_client::commands::{AlbumArt, AlbumArtEmbedded};
use std::env::var_os;
use tokio::join;

struct MpdConfig {
	host: String,
	password: Option<String>,
}

#[derive(Serialize, Debug)]
struct State {
	is_playing: bool,
	current_position: usize,
	queue: Vec<Song>,
	picture: Option<Picture>,
}

#[derive(Serialize, Debug)]
struct Song {
	title: Option<String>,
	artist: Option<String>,
	position: usize,
}

#[derive(Serialize, Debug, Clone)]
struct Picture {
	url: String,
	song_url: String,
}

impl State {
	fn new() -> Self {
		State {
			is_playing: false,
			current_position: 0,
			queue: Vec::new(),
			picture: None,
		}
	}
}

type SharedState = Arc<Mutex<State>>;

#[derive(Deserialize, Debug)]
enum ClientAction {
	Previous,
	Next,
	Pause,
	Play,
	Select {
		position: usize,
	},
}

trait Offsetable {
	fn offset(self, offset: usize) -> Self;
}

impl Offsetable for AlbumArt {
	fn offset(self, offset: usize) -> Self {
		self.offset(offset)
	}
}

impl Offsetable for AlbumArtEmbedded {
	fn offset(self, offset: usize) -> Self {
		self.offset(offset)
	}
}

async fn keep_state_updated(config: MpdConfig, state: SharedState, state_update_channel: Sender<()>, mut client_action_channel: UnboundedReceiver<ClientAction>) {
	let connection = TcpStream::connect(&config.host).await.expect("Can't connect to mpd host");

	let (client, mut state_changes) = Client::connect(connection).await.unwrap();

	client.password(config.password.clone()).await.unwrap();

	println!("Connected to mpd");

	update_state(state.clone(), &client).await;
	state_update_channel.send(()).unwrap();

	loop {
		tokio::select! {
			state_change = state_changes.next() => {
				match state_change.transpose().unwrap() {
					// something relevant changed
					Some(Subsystem::Player) => {
						update_state(state.clone(), &client).await;
						state_update_channel.send(()).unwrap();
					},
					// something changed but we don't care
					Some(_) => (),
					// connection was closed by the server
					None => (),
				}
			}
			action = client_action_channel.recv() => {
				if let Some(action) = action {
					let command = match action {
						ClientAction::Previous => client.command(commands::Previous).await,
						ClientAction::Next => client.command(commands::Next).await,
						ClientAction::Pause => client.command(commands::SetPause(true)).await,
						ClientAction::Play => client.command(commands::SetPause(false)).await,
						ClientAction::Select {position} => client.command(commands::Play::song(commands::SongPosition(position))).await,
					};

					command.unwrap();

					update_state(state.clone(), &client).await;
					state_update_channel.send(()).unwrap();
				}
			}
		}
	}
}

async fn update_state(state: SharedState, client: &Client) {
	let (status, queue, current_song) = join!(
		client.command(commands::Status),
		client.command(commands::Queue),
		client.command(commands::CurrentSong),
	);

	let status = status.unwrap();
	let queue = queue.unwrap();

	let picture = if let Ok(Some(current_song)) = current_song {
		get_song_picture(client, state.clone(), current_song.song.url.to_string()).await
	} else {
		None
	};

	let songs: Vec<Song> = queue
		.iter()
		.map(
			|song| -> Song {
				Song {
					artist: song.song.artists().first().cloned(),
					title: song.song.title().map(|title| title.to_string()),
					position: song.position.0,
				}
			}
		)
		.collect();

	{
		let state = &mut *state.lock().unwrap();
		state.is_playing = status.state == PlayState::Playing;
		state.current_position = status.current_song.map(|pos| pos.0.0).unwrap_or(0);
		state.queue = songs;
		state.picture = picture;
	}
}

async fn get_song_picture(client: &Client, state: SharedState, uri: String) -> Option<Picture> {
	// If we already have the picture for this song no need to get it a second time
	if let Some(ref picture) = state.lock().unwrap().picture {
		if picture.song_url == uri {
			return Some(picture.clone());
		}
	}

	let picture = get_full_song_picture(client, uri.clone(), commands::AlbumArt::new(uri.clone())).await;
	if picture.is_some() {
		return picture;
	}

	let picture = get_full_song_picture(client, uri.clone(), commands::AlbumArtEmbedded::new(uri.clone())).await;
	if picture.is_some() {
		return picture;
	}

	None
}

async fn get_full_song_picture<T>(client: &Client, uri: String, command: T) -> Option<Picture>
where T: commands::Command<Response=Option<responses::AlbumArt>> + Offsetable + Clone {
	if let Ok(Some(picture)) = client.command(command.clone()).await {
		let mut picture_data = picture.data().to_vec();
		let size = picture.size;
		let mime = picture.mime;

		while picture_data.len() < size {
			if let Ok(Some(p)) = client.command(command.clone().offset(picture_data.len())).await {
				picture_data.extend_from_slice(p.data());
			} else {
				return None;
			}
		}

		return Some(Picture {
			url: format!(
				"data:{};base64,{}",
				mime.clone().unwrap_or_else(|| "image/jpeg".to_string()),
				base64::encode(picture_data)
			),
			song_url: uri,
		});
	} else {
		None
	}
}

async fn handle_connection(state: SharedState, mut state_update_channel: Receiver<()>, client_action_channel: UnboundedSender<ClientAction>, raw_stream: TcpStream, addr: SocketAddr) -> TungsteniteResult<()> {
	println!("Incoming TCP connection from: {}", addr);

	let ws_stream = tokio_tungstenite::accept_async(raw_stream).await.expect("Error during the websocket handshake occurred");
	println!("WebSocket connection established: {}", addr);

	let (mut ws_sender, mut ws_receiver) = ws_stream.split();

	send_state(&state, &mut ws_sender).await?;

	loop {
		tokio::select! {
            msg = ws_receiver.next() => {
                match msg {
                    Some(msg) => {
                        let msg = msg?;
						match serde_json::from_str::<ClientAction>(msg.to_text()?) {
							Ok(action) => {
								client_action_channel.send(action).unwrap();
							}
							_ => println!("Unknown action found: {}", msg),
						}
                    }
                    None => break,
                }
            }
			_ = state_update_channel.recv() => {
				{
					send_state(&state, &mut ws_sender).await?;
				}
			}
        }
	}

	println!("{} disconnected", &addr);

	Ok(())
}

fn send_state<'a>(state: &SharedState, ws_sender: &'a mut SplitSink<WebSocketStream<tokio::net::TcpStream>, Message>) -> futures_util::sink::Send<'a, SplitSink<WebSocketStream<tokio::net::TcpStream>, Message>, Message> {
	let state_str: String = {
		let state: &State = &state.lock().unwrap();

		serde_json::to_string(state).unwrap()
	};

	ws_sender.send(Message::Text(state_str))
}

#[tokio::main]
async fn main() -> Result<(), IoError> {
	let mpd_config = MpdConfig {
		host: var_os("MPD_HOST").map(|host| host.into_string().unwrap()).unwrap_or_else(|| "localhost:6600".to_string()),
		password: var_os("MPD_PASSWORD").map(|host| host.into_string().unwrap()),
	};

	let addr = env::args().nth(1).unwrap_or_else(|| "0.0.0.0:8080".to_string());

	let state = SharedState::new(Mutex::new(State::new()));

	let (state_update_tx, _state_update_rx) = channel(1);
	let (client_actions_tx, client_actions_rx) = unbounded_channel();

	tokio::spawn(keep_state_updated(mpd_config, state.clone(), state_update_tx.clone(), client_actions_rx));

	// Create the event loop and TCP listener we'll accept connections on.
	let try_socket = TcpListener::bind(&addr).await;
	let listener = try_socket.expect("Failed to bind");
	println!("Listening on: {}", addr);

	// Let's spawn the handling of each connection in a separate task.
	while let Ok((stream, addr)) = listener.accept().await {
		tokio::spawn(handle_connection(state.clone(), state_update_tx.subscribe(), client_actions_tx.clone(), stream, addr));
	}

	Ok(())
}