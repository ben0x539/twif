use {
    std::{
        str::FromStr,
        io,
        net,
    },
    url::Url,
    tracing::{debug, instrument},
    spandoc::{spandoc},
};

#[instrument]
fn access_token_url(channel_name: &str) -> Url {
    let mut u = Url::parse("https://api.twitch.tv/api/channels").unwrap();
    u.path_segments_mut().unwrap()
        .push(channel_name)
        .push("access_token");
    debug!(url = %u, "constructing access token url");
    u
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct AccessTokenResponse {
    token: String,
    sig: String,
}

#[instrument]
fn master_manifest_url(channel_name: &str, sig: &str, token: &str) -> Url {
    // TODO: downcase channel_name before here
    let mut u = Url::parse_with_params(
        "https://usher.ttvnw.net/api/channel/hls",
        &[("sig", sig), ("token", token)]).unwrap();
    u.path_segments_mut().unwrap()
        .push(&format!("{}.m3u8", channel_name));
    debug!(url = %u, "constructing master manifest url");
    u
}

#[derive(Debug, structopt::StructOpt)]
struct Args {

    #[structopt(long = "listen-addr", name = "ADDR", default_value = "127.0.0.1:8080")]
    listen_addr: net::SocketAddr,
}

#[paw::main]
#[spandoc]
fn main(args: Args) -> eyre::Result<()> {
    use tracing_subscriber::{Registry, EnvFilter, layer::Layer};

    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();

    let fmt = tracing_subscriber::fmt::Layer::builder()
        .with_writer(io::stderr)
        .finish();
    let subscriber = filter//tracing_error::ErrorLayer::default()
        //.and_then(filter)
        .and_then(fmt)
        //.with_subscriber(fmt);
        .with_subscriber(Registry::default());

    /// Setting up default tracing subscriber
    tracing::subscriber::set_global_default(subscriber)?;

    run_hyper_service(&args.listen_addr)?;

    Ok(())
}

fn run_hyper_service(listen_addr: &net::SocketAddr) -> eyre::Result<()> {
    use hyper::{Body, Error, Request, Response};
    use hyper::service::{make_service_fn, service_fn};

    let make_svc = make_service_fn(|_| async {
        Ok::<_, Error>(service_fn(|req: Request<Body>| async move {
            Ok::<_, Error>(match start_stream(req).await {
                Ok(response) => response,
                Err(e) => Response::new(Body::from(e.to_string()))
            })
        }))
    });

    let mut runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        hyper::Server::try_bind(listen_addr)?
            .serve(make_svc).await?;
        Ok::<_, eyre::ErrReport>(())
    })?;

    Ok(())
}

async fn start_stream(req: hyper::Request<hyper::Body>)
        -> eyre::Result<hyper::Response<hyper::Body>> {
    use hyper::{Body, body::Bytes, Response};
    let mut segments = req.uri().path().split('/').skip(1);
    let channel_name = segments.next().unwrap_or("hungry");
    let minimum_resolution = segments.next()
        .and_then(|s| u32::from_str(s).ok()).unwrap_or(0);
    let playlist_url =
        get_variant_playlist_url(channel_name, minimum_resolution).await?;

    use tokio::process::Command;
    use std::process::Stdio;
    let mut child = Command::new("ffmpeg")
        .kill_on_drop(true)
        .args(&[
            "-loglevel", "error",
            "-i", &playlist_url,
            "-f", "gif",
            "-"])
        .stdout(Stdio::piped())
        .spawn()?;

    let (mut sender, body) = Body::channel();

    let mut child_output = child.stdout.take().unwrap();

    let copy_from_ffmpeg_to_client = async move {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 8*1024];

        loop {
            let n = child_output.read(&mut buf).await?;
            if n > 0 {
                let bytes = Bytes::copy_from_slice(&buf[..n]);
                sender.send_data(bytes).await?;
            }
        }
        Ok::<_, eyre::ErrReport>(())
    };

    tokio::task::spawn(async move {
        tokio::pin!(copy_from_ffmpeg_to_client);
        tokio::select! {
            _ = &mut copy_from_ffmpeg_to_client => {
                child.kill()?;
                child.await?;
            },
            _ = &mut child => {
                copy_from_ffmpeg_to_client.await?;
            },
        }
        Ok::<_, eyre::ErrReport>(())
    });

    let response = Response::builder()
       .header("Content-Type", "image/gif")
       .body(body)?;
    Ok(response)
}

#[spandoc]
#[instrument]
async fn get_variant_playlist_url(channel_name: &str, minimum_resolution: u32)
        -> eyre::Result<String> {
    let http = reqwest::Client::new();
    /// Requesting access token
    let result = http.get(access_token_url(channel_name))
        .header("Client-ID", "jzkbprff40iqj646a697cyrvl0zt2m6")
        .send().await?
        .error_for_status()?;
    /// Parsing access token json
    let access_token_response: AccessTokenResponse = result.json().await?;
    debug!(?access_token_response, "retrieved access token");

    /// Making master manifest request
    let result = http.get(master_manifest_url(channel_name,
            &access_token_response.sig, &access_token_response.token))
        .send().await?
        .error_for_status()?;
    /// Reading master manifest
    let master_manifest_response = result.text().await?;
    debug!(?master_manifest_response, "retrieved master manifest");
    /// Parsing master manifest
    let master_manifest =
        hls_m3u8::MasterPlaylist::from_str(&master_manifest_response)?;
    let mut renditions: Vec<_> = master_manifest.stream_inf_tags().into();
    renditions.sort_by_key(|s| s.resolution().map(|r| r.height));
    for rendition in &renditions {
        let resolution = rendition.resolution();
        debug!(?resolution, "has rendition");
    }

    let highest_rendition = renditions.pop();
    let rendition = renditions.into_iter()
        .find(|s| s.resolution().map(|r|r.height as u32 >= minimum_resolution)
            .unwrap_or(false))
        .or(highest_rendition)
        .ok_or_else(|| eyre::err!("no rendition available"))?;
    debug!(?rendition, "selected rendition");

    Ok(rendition.uri().to_string())
}
