//! End-to-end tests for the Spotify layer, driven against a mock API server.
//! These cover the request shapes and error mapping that unit tests can't reach.

use serde_json::json;
use spotify_control::controller::{Controller, Feedback};
use spotify_control::hotkeys::Action;
use spotify_control::spotify::{SpotifyClient, SpotifyError, StaticToken, TokenProvider};
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn device_json(id: &str, name: &str, volume: u8, supports_volume: bool) -> serde_json::Value {
    json!({
        "id": id,
        "name": name,
        "is_active": true,
        "is_restricted": false,
        "volume_percent": volume,
        "supports_volume": supports_volume,
    })
}

fn playing(device: serde_json::Value, is_playing: bool) -> serde_json::Value {
    json!({ "device": device, "is_playing": is_playing })
}

fn controller(server: &MockServer, step: u8) -> Controller<StaticToken> {
    let client = SpotifyClient::with_base_url(StaticToken("test-token".into()), server.uri());
    Controller::new(client, step)
}

/// GET /v1/me/player returning an active device at `volume`.
async fn mock_player(server: &MockServer, volume: u8, is_playing: bool) {
    Mock::given(method("GET"))
        .and(path("/v1/me/player"))
        .respond_with(ResponseTemplate::new(200).set_body_json(playing(
            device_json("dev1", "Desktop", volume, true),
            is_playing,
        )))
        .mount(server)
        .await;
}

// ------------------------------------------------------------------ volume ---

#[tokio::test]
async fn volume_up_reads_current_level_then_writes_the_step() {
    let server = MockServer::start().await;
    mock_player(&server, 50, true).await;

    Mock::given(method("PUT"))
        .and(path("/v1/me/player/volume"))
        .and(query_param("volume_percent", "55"))
        .and(query_param("device_id", "dev1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    controller(&server, 5).handle(Action::VolumeUp).await.unwrap();
}

#[tokio::test]
async fn volume_down_applies_a_negative_step() {
    let server = MockServer::start().await;
    mock_player(&server, 50, true).await;

    Mock::given(method("PUT"))
        .and(path("/v1/me/player/volume"))
        .and(query_param("volume_percent", "42"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    controller(&server, 8)
        .handle(Action::VolumeDown)
        .await
        .unwrap();
}

#[tokio::test]
async fn volume_clamps_to_100_instead_of_overflowing() {
    let server = MockServer::start().await;
    mock_player(&server, 98, true).await;

    Mock::given(method("PUT"))
        .and(path("/v1/me/player/volume"))
        .and(query_param("volume_percent", "100"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    controller(&server, 5).handle(Action::VolumeUp).await.unwrap();
}

#[tokio::test]
async fn volume_at_the_rail_skips_the_write_entirely() {
    let server = MockServer::start().await;
    mock_player(&server, 100, true).await;

    // No PUT mock is mounted: any write would 404 and fail the test.
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/volume"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&server)
        .await;

    controller(&server, 5).handle(Action::VolumeUp).await.unwrap();
}

#[tokio::test]
async fn repeated_volume_presses_reuse_the_cached_level() {
    let server = MockServer::start().await;

    // The level is read once; subsequent presses must ramp from the cache.
    Mock::given(method("GET"))
        .and(path("/v1/me/player"))
        .respond_with(ResponseTemplate::new(200).set_body_json(playing(
            device_json("dev1", "Desktop", 50, true),
            true,
        )))
        .expect(1)
        .mount(&server)
        .await;

    for expected in ["55", "60", "65"] {
        Mock::given(method("PUT"))
            .and(path("/v1/me/player/volume"))
            .and(query_param("volume_percent", expected))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
    }

    let c = controller(&server, 5);
    for _ in 0..3 {
        c.handle(Action::VolumeUp).await.unwrap();
    }
}

#[tokio::test]
async fn refuses_volume_on_a_device_that_cannot_change_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/me/player"))
        .respond_with(ResponseTemplate::new(200).set_body_json(playing(
            device_json("tv1", "Living Room TV", 30, false),
            true,
        )))
        .mount(&server)
        .await;

    let err = controller(&server, 5)
        .handle(Action::VolumeUp)
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("Living Room TV"), "got: {msg}");
    assert!(msg.contains("volume"), "got: {msg}");
}

// ------------------------------------------------------------- transport ---

#[tokio::test]
async fn every_request_carries_the_bearer_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/me/player/next"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    mock_player(&server, 50, true).await;

    controller(&server, 5)
        .handle(Action::NextTrack)
        .await
        .unwrap();
}

/// Rotates its token only when told to invalidate — so a retry that skips
/// invalidation presents the *same* token and fails the test.
struct RotatingToken {
    generation: AtomicUsize,
}

impl TokenProvider for RotatingToken {
    async fn access_token(&self) -> anyhow::Result<String> {
        Ok(format!("token-{}", self.generation.load(Ordering::SeqCst)))
    }
    async fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_401_triggers_exactly_one_retry_with_a_fresh_token() {
    let server = MockServer::start().await;

    // First call is rejected...
    Mock::given(method("POST"))
        .and(path("/v1/me/player/next"))
        .and(header("authorization", "Bearer token-0"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    // ...and the retry presents the refreshed token.
    Mock::given(method("POST"))
        .and(path("/v1/me/player/next"))
        .and(header("authorization", "Bearer token-1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let client = SpotifyClient::with_base_url(
        RotatingToken { generation: AtomicUsize::new(0) },
        server.uri(),
    );
    client.next_track(None).await.unwrap();
}

#[tokio::test]
async fn a_persistent_401_surfaces_as_unauthorized() {
    let server = MockServer::start().await;
    // A provider that can't produce a new token replays the same bearer, which
    // is the shape the retry must not silently rely on.
    Mock::given(method("POST"))
        .and(path("/v1/me/player/next"))
        .and(header("authorization", "Bearer t"))
        .respond_with(ResponseTemplate::new(401))
        .expect(2) // original + one retry, then give up
        .mount(&server)
        .await;

    let client = SpotifyClient::with_base_url(StaticToken("t".into()), server.uri());
    assert!(matches!(
        client.next_track(None).await.unwrap_err(),
        SpotifyError::Unauthorized
    ));
}

#[tokio::test]
async fn a_403_reports_the_spotify_message() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/pause"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": { "status": 403, "message": "Player command failed: Premium required" }
        })))
        .mount(&server)
        .await;

    let client = SpotifyClient::with_base_url(StaticToken("t".into()), server.uri());
    let err = client.pause(None).await.unwrap_err();
    assert!(format!("{err}").contains("Premium required"), "got: {err}");
}

#[tokio::test]
async fn a_404_maps_to_no_active_device() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/me/player/next"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let client = SpotifyClient::with_base_url(StaticToken("t".into()), server.uri());
    assert!(matches!(
        client.next_track(None).await.unwrap_err(),
        SpotifyError::NoActiveDevice
    ));
}

#[tokio::test]
async fn a_429_preserves_the_retry_after_delay() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/me/player/next"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
        .mount(&server)
        .await;

    let client = SpotifyClient::with_base_url(StaticToken("t".into()), server.uri());
    match client.next_track(None).await.unwrap_err() {
        SpotifyError::RateLimited(secs) => assert_eq!(secs, 30),
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn non_json_error_bodies_do_not_panic() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/me/player/next"))
        .respond_with(ResponseTemplate::new(502).set_body_string("<html>Bad Gateway</html>"))
        .mount(&server)
        .await;

    let client = SpotifyClient::with_base_url(StaticToken("t".into()), server.uri());
    match client.next_track(None).await.unwrap_err() {
        SpotifyError::Api { status, message } => {
            assert_eq!(status, 502);
            assert!(message.contains("Bad Gateway"));
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

// ------------------------------------------------------------- play/pause ---

#[tokio::test]
async fn play_pause_pauses_while_playing() {
    let server = MockServer::start().await;
    mock_player(&server, 50, true).await;

    Mock::given(method("PUT"))
        .and(path("/v1/me/player/pause"))
        .and(query_param("device_id", "dev1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    controller(&server, 5).handle(Action::PlayPause).await.unwrap();
}

#[tokio::test]
async fn play_pause_resumes_while_paused() {
    let server = MockServer::start().await;
    mock_player(&server, 50, false).await;

    Mock::given(method("PUT"))
        .and(path("/v1/me/player/play"))
        .and(query_param("device_id", "dev1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    controller(&server, 5).handle(Action::PlayPause).await.unwrap();
}

#[tokio::test]
async fn play_pause_adopts_a_device_when_the_session_has_none() {
    let server = MockServer::start().await;

    // 200 with a null device: the session outlived the client that owned it.
    // Playing without adopting a device first would just 404.
    Mock::given(method("GET"))
        .and(path("/v1/me/player"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "device": null, "is_playing": false })),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/me/player/devices"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "devices": [{
                "id": "idle1", "name": "Phone", "is_active": false,
                "is_restricted": false, "volume_percent": 40, "supports_volume": true
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/v1/me/player"))
        .and(body_json(json!({ "device_ids": ["idle1"], "play": false })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    // Targeted at the device we just adopted. A transfer does not make a device
    // active immediately, so an untargeted play 404s as "no active device".
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/play"))
        .and(query_param("device_id", "idle1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    controller(&server, 5).handle(Action::PlayPause).await.unwrap();
}

#[tokio::test]
async fn previous_track_hits_the_previous_endpoint() {
    let server = MockServer::start().await;
    mock_player(&server, 50, true).await;

    Mock::given(method("POST"))
        .and(path("/v1/me/player/previous"))
        .and(query_param("device_id", "dev1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    controller(&server, 5)
        .handle(Action::PreviousTrack)
        .await
        .unwrap();
}

// -------------------------------------------------- idle device adoption ---

#[tokio::test]
async fn adopts_an_idle_device_when_nothing_is_active() {
    let server = MockServer::start().await;

    // 204 = Spotify knows of no active session.
    Mock::given(method("GET"))
        .and(path("/v1/me/player"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/me/player/devices"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "devices": [{
                "id": "idle1", "name": "Phone", "is_active": false,
                "is_restricted": false, "volume_percent": 40, "supports_volume": true
            }]
        })))
        .mount(&server)
        .await;

    // Transfer must not start playback for a volume change.
    Mock::given(method("PUT"))
        .and(path("/v1/me/player"))
        .and(body_json(json!({ "device_ids": ["idle1"], "play": false })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/v1/me/player/volume"))
        .and(query_param("volume_percent", "45"))
        .and(query_param("device_id", "idle1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    controller(&server, 5).handle(Action::VolumeUp).await.unwrap();
}

// ------------------------------------------------- coalesced volume bursts ---

#[tokio::test]
async fn a_burst_of_detents_becomes_a_single_write() {
    let server = MockServer::start().await;
    mock_player(&server, 50, true).await;

    // Four detents at 5% each, in one request rather than four.
    Mock::given(method("PUT"))
        .and(path("/v1/me/player/volume"))
        .and(query_param("volume_percent", "70"))
        .and(query_param("device_id", "dev1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let feedback = controller(&server, 5).nudge_volume_by(4).await.unwrap();
    assert_eq!(feedback, Feedback::Volume(70));
}

#[tokio::test]
async fn a_burst_that_nets_downwards_writes_the_lower_level() {
    let server = MockServer::start().await;
    mock_player(&server, 50, true).await;

    Mock::given(method("PUT"))
        .and(path("/v1/me/player/volume"))
        .and(query_param("volume_percent", "40"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    // Spun down five, back up three.
    let feedback = controller(&server, 5).nudge_volume_by(-2).await.unwrap();
    assert_eq!(feedback, Feedback::Volume(40));
}

#[tokio::test]
async fn a_burst_big_enough_to_overshoot_clamps_to_the_rail() {
    let server = MockServer::start().await;
    mock_player(&server, 50, true).await;

    Mock::given(method("PUT"))
        .and(path("/v1/me/player/volume"))
        .and(query_param("volume_percent", "100"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    // A fast spin can easily exceed the remaining headroom.
    let feedback = controller(&server, 5).nudge_volume_by(40).await.unwrap();
    assert_eq!(feedback, Feedback::Volume(100));
}

#[tokio::test]
async fn detents_that_cancel_out_write_nothing() {
    let server = MockServer::start().await;
    mock_player(&server, 50, true).await;

    // No volume mock: an equal spin each way must not touch the API at all,
    // but should still report the level so the readout confirms the input.
    let feedback = controller(&server, 5).nudge_volume_by(0).await.unwrap();
    assert_eq!(feedback, Feedback::Volume(50));
}

#[tokio::test]
async fn the_volume_readout_clears_sooner_than_a_skip() {
    // Volume arrives in bursts and is self-evident; a skip deserves a beat.
    assert!(Feedback::Volume(50).hold() < Feedback::NextTrack.hold());
    assert!(Feedback::Volume(50).hold() < Feedback::Paused.hold());
}

#[tokio::test]
async fn restricted_devices_are_never_adopted() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/me/player"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/me/player/devices"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "devices": [
                { "id": "tv", "name": "TV", "is_active": true, "is_restricted": true,
                  "volume_percent": 20, "supports_volume": true },
                { "id": "pc", "name": "PC", "is_active": false, "is_restricted": false,
                  "volume_percent": 60, "supports_volume": true }
            ]
        })))
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/v1/me/player"))
        .and(body_json(json!({ "device_ids": ["pc"], "play": false })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/v1/me/player/volume"))
        .and(query_param("volume_percent", "65"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    controller(&server, 5).handle(Action::VolumeUp).await.unwrap();
}
