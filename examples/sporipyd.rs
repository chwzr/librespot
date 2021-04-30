use std::env;
use std::fs;
use std::fs::File;

use librespot::core::authentication::Credentials;
use librespot::core::config::SessionConfig;
use librespot::core::session::Session;
use librespot::core::spotify_id::SpotifyId;
use librespot::playback::audio_backend;
use librespot::playback::config::{AudioFormat, PlayerConfig};
use librespot::playback::player::Player;
use librespot::metadata::{Metadata, Track, Artist, Album};

#[tokio::main]
async fn main() {
    let session_config = SessionConfig::default();
    let player_config = PlayerConfig::default();
    let audio_format = AudioFormat::default();

    let args: Vec<_> = env::args().collect();
    if args.len() != 4 {
        eprintln!("Usage: {} USERNAME PASSWORD TRACK", args[0]);
        return;
    }
    let credentials = Credentials::with_password(&args[1], &args[2]);

    let track = SpotifyId::from_base62(&args[3]).unwrap();


    

    let backend = audio_backend::find(std::option::Option::Some(String::from("pipe"))).unwrap();

    // clean output file first
    fs::remove_file("./tmp/out.ogg");
    File::create("./tmp/out.ogg");


    println!("Connecting ..");
    let session = Session::connect(session_config, credentials, None)
        .await
        .unwrap();

    let track_meta = Track::get(&session, track).await.unwrap();
    println!("name:{}", track_meta.name);
    
    let album = Album::get(&session, track_meta.album).await.unwrap();
    println!("album:{}", album.name);
    println!("cover:{}", album.covers[0]);

    for artist_id in album.artists {
        let artist = Artist::get(&session, artist_id).await.unwrap();
        println!("album_artist_:{}",artist.name);
    }

    for artist_id in track_meta.artists {
        let artist = Artist::get(&session, artist_id).await.unwrap();
        println!("artist:{}",artist.name);
    }


    let (mut player, _) = Player::new(player_config, session, None, move || {
        backend(std::option::Option::Some(String::from("./tmp/out.ogg")), audio_format)
    });

    player.load(track, true, 0);


    println!("Playing...");

    player.await_end_of_track().await;

    println!("Done");
}
