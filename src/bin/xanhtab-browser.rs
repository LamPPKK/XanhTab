use std::{fs, path::PathBuf, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    time::timeout,
};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use url::Url;
use uuid::Uuid;
use xanhtab::{
    browser::{BrowserCommand, BrowserResponse},
    model::{EgressMode, NavigationCommand, StreamProfile},
};

#[derive(Parser)]
#[command(version, about = "XanhTab WPE/GStreamer process bridge")]
struct Args {
    #[arg(long, default_value = "gst-launch-1.0")]
    gst_launch: PathBuf,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, default_value = "/run/xanhtab/browser.sock")]
    socket: PathBuf,
    #[arg(long, default_value = "/usr/share/xanhtab/web/internal-home.html")]
    internal_home_file: PathBuf,
    #[arg(long, default_value = "127.0.0.1")]
    signaling_host: String,
    #[arg(long, default_value_t = 8444)]
    signaling_port: u16,
    /// Explicitly allow webrtcsink's public STUN default. Disabled in production.
    #[arg(long)]
    public_stun: bool,
    /// Use newline-delimited JSON over stdin for the hardware gate harness.
    #[arg(long)]
    stdio: bool,
    /// Helper-side command deadline; kept below the daemon IPC timeout.
    #[arg(long, default_value_t = 1_500)]
    command_timeout_ms: u64,
}

struct Pipeline {
    child: Option<Child>,
    session_id: Option<Uuid>,
    profile: StreamProfile,
    egress: EgressMode,
    history: Vec<Url>,
    position: usize,
}

impl Default for Pipeline {
    fn default() -> Self {
        Self {
            child: None,
            session_id: None,
            profile: StreamProfile::FullHd30,
            egress: EgressMode::Direct,
            history: Vec::new(),
            position: 0,
        }
    }
}

impl Pipeline {
    async fn apply(&mut self, command: BrowserCommand, args: &Args) -> Result<()> {
        match command {
            BrowserCommand::Start {
                session_id,
                url,
                stream_profile,
                egress,
            } => {
                self.stop().await?;
                self.session_id = Some(session_id);
                self.profile = stream_profile;
                self.egress = egress;
                self.history = vec![url.clone()];
                self.position = 0;
                self.spawn(url, args).await?;
            }
            BrowserCommand::Navigate { navigation } => {
                let next = match navigation {
                    NavigationCommand::Navigate { url } => {
                        self.history.truncate(self.position + 1);
                        self.history.push(url.clone());
                        self.position = self.history.len() - 1;
                        Some(url)
                    }
                    NavigationCommand::Back if self.position > 0 => {
                        self.position -= 1;
                        Some(self.history[self.position].clone())
                    }
                    NavigationCommand::Forward if self.position + 1 < self.history.len() => {
                        self.position += 1;
                        Some(self.history[self.position].clone())
                    }
                    NavigationCommand::Reload => self.history.get(self.position).cloned(),
                    NavigationCommand::Stop => {
                        self.stop().await?;
                        None
                    }
                    _ => None,
                };
                if let Some(url) = next {
                    self.stop().await?;
                    self.spawn(url, args).await?;
                }
            }
            BrowserCommand::Stop => {
                self.stop().await?;
            }
        }
        Ok(())
    }

    async fn spawn(&mut self, url: Url, args: &Args) -> Result<()> {
        let location = resolve_location(&url, &args.internal_home_file)?;
        let gst_args = pipeline_args(
            &location,
            self.profile,
            self.egress,
            &args.signaling_host,
            args.signaling_port,
            args.public_stun,
        )?;
        if args.dry_run {
            println!("{} {}", args.gst_launch.display(), gst_args.join(" "));
            return Ok(());
        }
        let mut command = Command::new(&args.gst_launch);
        command
            .args(&gst_args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(proxy) = proxy_for(self.egress)? {
            command
                .env("http_proxy", &proxy)
                .env("https_proxy", &proxy)
                .env("all_proxy", &proxy);
        }
        self.child = Some(
            command
                .spawn()
                .context("failed to launch GStreamer pipeline")?,
        );
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        Ok(())
    }
}

fn pipeline_args(
    url: &Url,
    profile: StreamProfile,
    _egress: EgressMode,
    signaling_host: &str,
    signaling_port: u16,
    public_stun: bool,
) -> Result<Vec<String>> {
    if !matches!(url.scheme(), "http" | "https" | "file") {
        bail!("unsupported browser URL scheme")
    }
    let signaling_is_loopback = signaling_host == "localhost"
        || signaling_host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if !signaling_is_loopback || signaling_port == 0 {
        bail!("embedded signaling server must bind a loopback host and non-zero port")
    }
    let (width, height, fps) = profile.dimensions();
    let bitrate = match profile {
        StreamProfile::FullHd30 => 6_000_000,
        StreamProfile::Hd15 => 3_000_000,
        StreamProfile::Sd10 => 1_200_000,
    };
    let mut arguments = vec![
        "-e".into(),
        "wpesrc".into(),
        "name=web".into(),
        format!("location={url}"),
        "web.video".into(),
        "!".into(),
        "queue".into(),
        "leaky=downstream".into(),
        "max-size-buffers=2".into(),
        "!".into(),
        "gldownload".into(),
        "!".into(),
        "videoconvert".into(),
        "!".into(),
        format!("video/x-raw,format=I420,width={width},height={height},framerate={fps}/1"),
        "!".into(),
        "v4l2h264enc".into(),
        format!("extra-controls=controls,video_bitrate={bitrate}"),
        "!".into(),
        "h264parse".into(),
        "config-interval=-1".into(),
        "!".into(),
        "video/x-h264,profile=baseline".into(),
        "!".into(),
        "webrtcsink".into(),
        "name=rtc".into(),
        "enable-control-data-channel=true".into(),
        "run-signalling-server=true".into(),
        format!("signalling-server-host={signaling_host}"),
        format!("signalling-server-port={signaling_port}"),
        "run-web-server=false".into(),
        "meta=meta,name=xanhtab".into(),
        "web.audio_0".into(),
        "!".into(),
        "queue".into(),
        "!".into(),
        "audioconvert".into(),
        "!".into(),
        "audioresample".into(),
        "!".into(),
        "opusenc".into(),
        "bitrate=64000".into(),
        "!".into(),
        "rtc.".into(),
    ];
    if !public_stun {
        arguments.push("stun-server=".into());
    }
    Ok(arguments)
}

fn resolve_location(url: &Url, internal_home_file: &std::path::Path) -> Result<Url> {
    if url.scheme() != "xanhtab" {
        return Ok(url.clone());
    }
    if url.host_str() != Some("home") {
        bail!("unsupported internal XanhTab route")
    }
    Url::from_file_path(internal_home_file)
        .map_err(|_| anyhow::anyhow!("internal home path must be absolute"))
}

fn proxy_for(mode: EgressMode) -> Result<Option<String>> {
    match mode {
        EgressMode::Tor => Ok(Some("socks5h://127.0.0.1:9050".into())),
        EgressMode::Warp => Ok(Some("socks5h://127.0.0.1:40000".into())),
        EgressMode::Proxy => {
            let path = std::env::var_os("XANHTAB_PROXY_URL_FILE")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/etc/xanhtab/secrets/proxy-url"));
            let value = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let parsed = Url::parse(value.trim()).context("invalid proxy URL")?;
            if !matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h") {
                bail!("unsupported proxy scheme")
            }
            Ok(Some(parsed.to_string()))
        }
        _ => Ok(None),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "xanhtab=info".into()),
        )
        .compact()
        .init();
    let args = Args::parse();
    if args.command_timeout_ms == 0 || args.command_timeout_ms > 30_000 {
        bail!("command timeout must be between 1 and 30000 milliseconds");
    }
    if args.stdio {
        return run_stdio(&args).await;
    }
    if let Some(parent) = args.socket.parent() {
        fs::create_dir_all(parent)?;
    }
    if args.socket.exists() {
        fs::remove_file(&args.socket)?;
    }
    let listener = UnixListener::bind(&args.socket)
        .with_context(|| format!("failed to bind {}", args.socket.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&args.socket, fs::Permissions::from_mode(0o660))?;
    }
    info!(socket = %args.socket.display(), "xanhtab-browser ready");
    let mut pipeline = Pipeline::default();
    loop {
        let (stream, _) = listener.accept().await?;
        if let Err(error) = handle(stream, &mut pipeline, &args).await {
            error!(%error, "browser command failed");
        }
    }
}

async fn run_stdio(args: &Args) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut pipeline = Pipeline::default();
    let deadline = Duration::from_millis(args.command_timeout_ms);
    while let Some(line) = lines.next_line().await? {
        let command: BrowserCommand =
            serde_json::from_str(&line).context("invalid bridge command")?;
        timeout(deadline, pipeline.apply(command, args))
            .await
            .context("browser command timed out")??;
    }
    timeout(deadline, pipeline.stop())
        .await
        .context("browser stop timed out")??;
    Ok(())
}

async fn handle(stream: UnixStream, pipeline: &mut Pipeline, args: &Args) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    let deadline = Duration::from_millis(args.command_timeout_ms);
    timeout(deadline, BufReader::new(reader).read_line(&mut line))
        .await
        .context("browser request read timed out")??;
    let response = match serde_json::from_str::<BrowserCommand>(&line) {
        Ok(command) => match timeout(deadline, pipeline.apply(command, args)).await {
            Ok(Ok(())) => BrowserResponse {
                ok: true,
                detail: "browser command applied".into(),
            },
            Ok(Err(error)) => BrowserResponse {
                ok: false,
                detail: error.to_string(),
            },
            Err(_) => {
                let _ = timeout(deadline, pipeline.stop()).await;
                *pipeline = Pipeline::default();
                BrowserResponse {
                    ok: false,
                    detail: "browser command timed out".into(),
                }
            }
        },
        Err(error) => BrowserResponse {
            ok: false,
            detail: format!("invalid browser command: {error}"),
        },
    };
    let mut payload = serde_json::to_vec(&response)?;
    payload.push(b'\n');
    writer.write_all(&payload).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_uses_video_element_path_and_hardware_encoder() {
        let args = pipeline_args(
            &Url::parse("https://example.com").unwrap(),
            StreamProfile::Hd15,
            EgressMode::Direct,
            "127.0.0.1",
            8444,
            false,
        )
        .unwrap();
        assert!(args.contains(&"v4l2h264enc".to_string()));
        assert!(
            args.contains(
                &"video/x-raw,format=I420,width=1280,height=720,framerate=15/1".to_string()
            )
        );
        assert!(args.contains(&"enable-control-data-channel=true".to_string()));
        assert!(args.contains(&"signalling-server-host=127.0.0.1".to_string()));
        assert!(args.contains(&"signalling-server-port=8444".to_string()));
        assert!(args.contains(&"run-web-server=false".to_string()));
        assert!(args.contains(&"stun-server=".to_string()));
    }

    #[test]
    fn internal_home_uses_a_read_only_packaged_route() {
        let location = resolve_location(
            &Url::parse("xanhtab://home").unwrap(),
            std::path::Path::new("/usr/share/xanhtab/web/internal-home.html"),
        )
        .unwrap();
        assert_eq!(
            location.as_str(),
            "file:///usr/share/xanhtab/web/internal-home.html"
        );
    }
}
