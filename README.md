# IC WebSocket Gateway

WebSockets enable web applications to maintain a full-duplex connection between the backend and the frontend. This allows for many different use-cases, such as notifications, dynamic content updates (e.g., showing new comments/likes on a post), collaborative editing, etc.

At the moment, the Internet Computer does not natively support WebSocket connections and developers need to resort to work-arounds in the frontend to enable a similar functionality. This results in a poor developer experience and an overload of the backend canister.

This repository contains the implementation of a WebSocket Gateway enabling clients to establish a full-duplex connection to their backend canister on the Internet Computer via the WebSocket API.

# Running the WS Gateway

## Prerequisites

Make sure you have the **Rust toolchain** installed. You can find instructions [here](https://www.rust-lang.org/tools/install).

## Standalone

1. Run the gateway:
    ```
    cargo run
    ```
2. After the gateway starts, it prints something like:

    ```
    2023-08-01T07:55:58.315793Z INFO ic_websocket_gateway::gateway_server: Gateway Agent principal: sqdfl-mr4km-2hfjy-gajqo-xqvh7-hf4mf-nra4i-3it6l-neaw4-soolw-tae
    ```

    This is the principal that the gateway uses to interact with the canister IC WebSocket CDK.

3. Copy the gateway principal as you will need it to initialize the canister CDK, so that the canister can authenticate the gateway. How to do this is explained in the [ic_websocket_example](https://github.com/omnia-network/ic_websocket_example#running-the-project-locally) README.

### Options available

There are some command line arguments that you can set when running the gateway:
| Argument | Description | Default |
| --- | --- | --- |
| `--gateway-address` | The **IP:port** on which the gateway will listen for incoming connections. | `0.0.0.0:8080` |
| `--subnet-url` | The URL of the IC subnet to which the gateway will connect. | `http://127.0.0.1:4943` |
| `--polling-interval` | The interval (in **milliseconds**) at which the gateway will poll the canisters for new messages. | `100` |
| `--tls-certificate-pem-path` | The path to the TLS certificate file. See [Obtain a TLS certificate](#obtain-a-tls-certificate) for more details. | _empty_ |
| `--tls-certificate-key-pem-path` | The path to the TLS private key file. See [Obtain a TLS certificate](#obtain-a-tls-certificate) for more details. | _empty_ |

## Docker

A [Dockerfile](./Dockerfile) is provided, together with a [docker-compose.yml](./docker-compose.yml) file to run the gateway. Make sure you have [Docker](https://docs.docker.com/get-docker/) and [Docker Compose](https://docs.docker.com/compose/install/) installed.

1. Set the environment variables:

    ```
    cp .env.example .env
    ```

2. The docker-compose.yml file is configured to make the gateway run with TLS enabled. For this, you need a public domain (that you will put in the `DOMAIN_NAME` environment variable) and a TLS certificate for that domain. See [Obtain a TLS certificate](#obtain-a-tls-certificate) for more details.
3. Open the `443` port (or the port that you set in the `LISTEN_PORT` environment variable) on your server and make it reachable from the Internet.
4. Run the gateway:
    ```
    docker compose up
    ```
5. The Gateway will print its principal in the container logs, just as explained above.
6. Whenever you want to rebuild the gateway image, run:
    ```
    docker compose up --build
    ```

### Obtain a TLS certificate

1. Buy a domain name and point it to the server where you are running the gateway.
2. Make sure the `.env` file is configured with the correct domain name, see above.
3. Obtain an SSL certificate for your domain:
    ```
    ./scripts/certbot_certonly.sh
    ```
    This will guide you in obtaining a certificate using [Certbot](https://certbot.eff.org/) in [Standalone mode](https://eff-certbot.readthedocs.io/en/stable/using.html#standalone).
    > Make sure you have port `80` open on your server and reachable from the Internet, otherwise certbot will not be able to verify your domain. Port `80` is used only for the certificate generation and can be closed afterwards.

To renew the SSL certificate, you can run the same command as above:

```
./scripts/certbot_certonly.sh
```

## Configure logging

The gateway uses the [tracing](https://docs.rs/tracing) crate for logging. There are two tracing outputs configured:

-   output to **stdout**, which has the `info` level and can be configured with the `RUST_LOG_STDOUT` env variable, see below;
-   output to a **file**, which is saved in the `data/traces/` folder and has the default `trace` level. The file name is `gateway_{start-timestamp}.log`. It can be configured with the `RUST_LOG_FILE` env variable, see below.

The `RUST_LOG` environment variable enables to set different levels for each module. See the [EnvFilter](https://docs.rs/tracing-subscriber/0.3.17/tracing_subscriber/filter/struct.EnvFilter.html) documentation for more details.
For example, to set the tracing level to `debug`, you can run:

```
RUST_LOG_FILE=ic_websocket_gateway=debug RUST_LOG_STDOUT=ic_websocket_gateway=debug cargo run
```

# Development

## Testing

### Unit tests

Some unit tests are provided in the [tests](./src/ic-websocket-gateway/src/tests) folder. You can run them with:

```
cargo test
```

### Integration tests (Rust test canister)

Integration tests require:

-   [Node.js](https://nodejs.org/en/download/) (version 16 or higher)
-   [dfx](https://internetcomputer.org/docs/current/developer-docs/setup/install), with which to run an [IC local replica](https://internetcomputer.org/docs/current/references/cli-reference/dfx-start/)
-   a test canister deployed on the local replica

After installing Node.js and dfx, you can run the integration tests as follows:

1. Run a local replica:

    ```
    dfx start --clean --background
    ```

2. Run the gateway:
    ```
    cargo run
    ```
3. Copy the principal printed on the terminal (as explained above)
4. Open a new terminal and install test dependencies:
    ```
    ./scripts/install_test_dependencies.sh
    ```
5. Move to the directory `tests/test_canister_rs` and deploy a test canister using the IC WebSocket CDK:
    ```
    dfx deploy test_canister_rs --argument '(opt "<gateway-principal-obtained-in-step-3>")'
    ```
6. Move to the directory `tests/integration` and set the environment variables:
    ```
    cp .env.example .env
    ```
7. Run the test:
    ```
    npm test
    ```

### Local test script

To make it easier to run both the unit and integration tests, a script is provided that runs the local replica, the gateway and the tests in sequence. You can run it with:

```
./scripts/local_test.sh
```

# How it works

## Overview

![](./docs/images/image2.png)

In order to enable WebSockets for a dapp running on the IC, we use a trustless intermediary, called WS Gateway, that provides a WebSocket endpoint for the frontend of the dapp, running in the user’s browser and interacts with the canister backend.

The gateway is needed as a WebSocket is a one-to-one connection between client and server, but the Internet Computer does not support that due to its replicated nature. The gateway relays all messages coming in via the WebSocket from the client as API canister calls for the backend and sends each message polled from the backend via the WebSocket with the corresponding client.

## Features

-   General: The WS Gateway can provide a WebSocket interface for many different dapps at the same time.
-   Trustless:

    -   all messages are signed: messages sent by the canister are [certified](https://internetcomputer.org/how-it-works/response-certification/); messages sent by the client signed using an identity either provided by the user or generated by the [IC WebSocket Frontend SDK](https://github.com/omnia-network/ic-websocket-sdk-js). This way, the gateway cannot tamper the content of the messages;
    -   all messages have a sequence number to guarantee all messages are received in the correct order;
    -   all messages are accompanied by a timestamp to prevent the gateway from delaying them;
    -   all messages are acknowledged by the [IC WebSocket Backend CDK](https://github.com/omnia-network/ic-websocket-cdk-rs) so that the gateway cannot block them;
    -   the [IC WebSocket Backend CDK](https://github.com/omnia-network/ic-websocket-cdk-rs) expects keep alive messages from each connected client so that it can detect if a client is not connected anymore;

-   IMPORTANT CAVEAT: NO ENCRYPTION!
    No single replica can be assumed to be trusted, so the canister state cannot be assumed to be kept secret. This means that when exchanging messages with the canister, we have to keep in mind that in principle the messages could be seen by others on the canister side.
    We could encrypt the messages between the client and the canister (so that they’re hidden from the gateway and any other party that cannot see the canister state), but we chose not to do so to make it clear that **in principle the messages could be seen by others on the canister side**.
    This will be solved in the next version of IC WebSocket using VetKeys.

## Components

1. Client:

    Client uses the [IC WebSocket Frontend SDK](https://github.com/omnia-network/ic-websocket-sdk-js) to establish the IC WebSocket connection mediated by the WS Gateway in order to communicate with the backend canister using the WebSocket API. When instantiating a new IC WebSocket connection, the client can pass an identity to the SDK in order to authenticate its messages to the canister.

    The client can instantiate a new IC WebSocket connection by calling the `IcWebSocket` constructor.

    When the client calls the `send` method of the SDK, the SDK creates a signed envelope with the content specified by the client and signed with its identity. The signed envelope is sent to the WS Gateway via WebSocket and specifies the `ws_open` method of the canister.

    Once receiving a message from the WS Gateway via WebSocket, the SDK validates the messages by verifying the certificate provided using the public key of the Internet Computer.

2. WS Gateway:

    WS Gateway accepts WebSocket connections with multiple clients in order to relay their messages to and from the canister.

    Upon receiving a signed envelope from a client, the WS Gateway relays the message to the `/<canister_id>/call` endpoint of the Internet Computer. This way, the WS Gateway is transparent to the canister, which receives the request as if sent directly by the client which signed it.

    In order to get updates from the canister, the WS Gateway polls the caniser by sending periodic queries to the `ws_get_messages` method. Upon receiving a response to a query, the WS Gateway relays the contained message and certificate to the corresponding client using the WebSocket.

3. Backend canister:

    The backend canister uses the [IC WebSocket Backend CDK](https://github.com/omnia-network/ic-websocket-cdk-rs) which exposes the methods of a typical WebSocket server (`ws_open`, `ws_message`, `ws_error`, `ws_close`) plus the `ws_get_messages` method polled by the WS Gateway. The backend canister must specify the callback functions which should be executed on different WebSocket events (`open`, `message`, `error`, `close`). The CDK triggers the corresponding callback upon receiving a request on one of the WebSocket methods.

    The CDK also implements the logic necessary to detect a possible WS Gateway misbehaviour.

    In order to send an update to one of its clients, the canister calls the `ws_send` method of the CDK specifying the message that it wants to be delivered to the client. The CDK pushes this message in a FIFO queue together with all the other clients' messages. Upon receiving a query call to the `ws_get_messages` method, the CDK returns the messages in this queue (up to a certain limit), together with a certificate which proves to the clients that the messages are actually the ones sent from the canister even if relayed by the WS Gateway.

# License

TODO: add a license

# Contributing

Feel free to open issues, pull requests and reach out to us!

## Massimo Albarello

-   [Linkedin](https://linkedin.com/in/massimoalbarello)
-   [Twitter](https://twitter.com/MaxAlbarello)
-   [Calendly](https://cal.com/massimoalbarello/meeting)
-   [Email](mez@omnia-network.com)

## Luca Bertelli

-   [Linkedin](https://www.linkedin.com/in/luca-bertelli-407041128/)
-   [Twitter](https://twitter.com/ilbert_luca)
-   [Calendly](https://cal.com/lucabertelli/)
-   [Email](liuc@omnia-network.com)
