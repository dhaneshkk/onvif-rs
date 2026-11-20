# ONVIF-rs

ONVIF-rs is a Rust client library implementation of the ONVIF specification.

## Features

- [x] all ONVIF types are generated from official schema
- [ ] all ONVIF operations are generated from official schema
- [x] operations are async (currently only _tokio_ runtime is supported)
- [x] device discovery on the local network using _WS-discovery_ which is mandatory for all ONVIF devices
- [x] authentication using _WS-Security UsernameToken_ which is mandatory for all ONVIF devices
- [x] zero unsafe

## Usage

Cargo.toml:

```toml
[dependencies]
onvif = { git = "https://github.com/lumeohq/onvif-rs" }
```

## Security defaults

- **HTTPS certificates are verified** (`tls` feature). To trust a camera's self-signed
  certificate, pin it with `ClientBuilder::add_root_certificate`. If the certificate doesn't list
  the camera's IP address, also set `danger_accept_invalid_hostnames(true)`.
  `danger_accept_invalid_certs(true)` turns verification off completely.
- **UsernameToken auth is not sent over plain HTTP.** Its SHA-1 password digest can be cracked
  offline. `AuthType::Any` uses only Digest over HTTP. For cameras that support nothing else, set
  `allow_username_token_over_http(true)`.
- **Redirects** keep the credentials on the same host, never go from HTTPS to HTTP, and are
  limited by `max_redirections` (default 1).
- **Response bodies** are capped at `max_response_bytes` (default 16 MiB).
- **Rejected logins are counted across clones of a client**, which stops it before a camera
  locks the account. Because of this, requests through one client run one at a time. Use
  `Client::with_own_auth_state()` for work that should run in parallel.

## Troubleshooting

The `tls` feature uses the system's OpenSSL through `native-tls`. If the build can't find it on
Debian or Ubuntu, install the development package:

```
sudo apt install pkg-config libssl-dev
```

## Examples

To [discover](onvif/examples/discovery.rs) devices on the local network:

```shell script
cargo run --example discovery
```

To [inspect and control a camera](onvif/examples/camera.rs):

```shell script
cargo run --example camera -- help

cargo run --example camera -- get-system-date-and-time \
    --uri=http://192.168.0.2:8000

cargo run --example camera -- set-hostname \
    --uri=http://192.168.0.2:8000 --username=admin --password=qwerty cam2

cargo run --example camera -- get-stream-uris --uri=http://192.168.0.2:8000
```

To [pull events](onvif/examples/event.rs) from a camera, adjust credentials in event.rs and run:

```shell script
cargo run --example event
```

## Dependencies

- XSD -> Rust code generation: [xsd-parser-rs](https://github.com/lumeohq/xsd-parser-rs)
- XML (de)serialization: [yaserde](https://github.com/media-io/yaserde)

## Contributing

Pull requests are welcome. For major changes, please open an issue first to discuss what you would like to change.

Please make sure to update tests as appropriate.

## License

[MIT](LICENSE)
