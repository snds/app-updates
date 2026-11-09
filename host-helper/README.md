# Host helper

One-click **Update** and **Reload** for the servers on the Unraid host (the unified updates
proposal, U5). A server's admin page asks; this helper, running beside it on the host, does it.

- **Update:** pull the server's image at an allowed tag, check its digest, and recreate the
  server's container on it (`docker compose up -d`).
- **Reload:** restart the server's containers (`docker compose restart`).
- **Status:** which image each container runs (tag, digest, version), and what an update is
  doing right now or how the last one ended.

It serves the Lockdown server (`ghcr.io/snds/lockdown-server`), the Vale gateway
(`ghcr.io/snds/vale-gateway`) and Warpcore (`ghcr.io/snds/warpcore-{web,api,sandbox}`, three
images updated together, with a backup before and a self-test after: see
[Several images, and steps around an update](#several-images-and-steps-around-an-update)).

## What it does not do

- It never runs a command a request names. There are three operations, and each runs fixed
  `docker` commands built from the allow-list. A stack's `before` and `after` steps are written
  in the allow-list too; a request only starts the update they belong to.
- It never touches a stack, service or image that isn't on the allow-list, and never pulls a
  tag the list doesn't allow (`main`, or a main-channel version such as
  `0.1.0-main.20261108093000`).
- It doesn't build from source, edit compose files, roll back on its own, or update itself. It
  takes a backup only when the stack's `before` step is one. (To go back, update to the older
  version tag, if the stack's data allows it.)
- It isn't reachable from the LAN or the internet. It publishes no port.

## How it is deployed, and why

**A container on the Unraid host, holding the Docker socket.** It is the only container that
does. Each server reaches it over its own internal Docker network (`internal: true`: no route
out), which holds just that server and the helper.

Why a container rather than a host binary run by Unraid's User Scripts:

- Docker restarts it, after a crash and after a reboot (`restart: unless-stopped`). User Scripts
  has no supervisor.
- It can sit on private Docker networks the servers already join; a host process would have to
  listen on a host address the containers can reach, which is also reachable from elsewhere.
- It updates like everything else (`docker compose pull && docker compose up -d`), with a
  pinned, reproducible image, and runs read-only with every Linux capability dropped.

The socket gives it root-level power over the host either way; what keeps that safe is how
little the helper agrees to do, below.

## Threat model, in plain words

**What it protects:** the host. Whoever controls Docker controls the machine. Only the helper
holds the socket; the servers never do.

| If… | Then… |
|---|---|
| Someone on the LAN or internet tries to reach it | They can't: no published port, and its networks are internal. |
| A server is broken into | The attacker has that server's key, so they can do what the list allows for **that stack only**: restart it, or run another published build of its own image from its own repository (an older main version too). They can't run a command, choose another image, mount a folder, or touch another stack. An older Lockdown build refuses a newer database, so the worst case is an outage you fix by updating again. |
| Someone replays a captured request | Refused. Every request is signed with the stack's key over the method, path, stack, time, a random nonce and the body; the helper accepts a time within 90 seconds of its clock and never the same nonce twice. |
| Someone answers in the helper's place | The server ignores it. Successful replies are signed with the same key over the request's nonce. |
| The registry or the CI that publishes an image is compromised | The helper would pull a bad image, just as `docker compose pull` would by hand. When the server passes the digest from its release manifest (`expect_digest`), any other image is refused and nothing restarts. Vale's manifest carries the digest; Lockdown's doesn't yet. |
| Something floods it | 30 signed requests a minute per stack; after 20 refused requests in a minute it refuses everything unsigned for the rest of that minute; 16 connections at once; small size limits; one update or reload per stack at a time. |
| The helper itself has a bug | It is small, has no shell, runs `docker` with an argument list (never through a shell) and a clean environment, and runs read-only with all capabilities dropped. It still holds the socket, so it is kept small on purpose. |

**Keys:** one per stack, 32 random bytes, made at install. The helper keeps it in
`/mnt/user/appdata/host-helper/config/keys/<stack>.key` (mode 600; it refuses a key others can
read); the server keeps it in its stack folder's `host-helper.env` (mode 600). Each request's
log line names the time, the peer, the stack, the operation, the tag and the result, never a
key or a signature.

## Install on Unraid (Sean)

Run these in the Unraid terminal as root. They assume the stacks live where the sample files
say; if yours are elsewhere, change the paths in the files and the commands the same way.

| Stack | Its folder (with `compose.yaml`) |
|---|---|
| Lockdown server | `/mnt/user/appdata/lockdown` |
| Vale gateway | `/mnt/user/appdata/vale/apps/gateway` (the checkout is `/mnt/user/appdata/vale`) |

Both must already run the published image (`image: ghcr.io/snds/...:main` in their
`compose.yaml`, which Lockdown PR #58 and Vale PR #75 set).

**1. Folders and files.**

```sh
mkdir -p /mnt/user/appdata/host-helper/config/keys
chmod 700 /mnt/user/appdata/host-helper/config /mnt/user/appdata/host-helper/config/keys
cd /mnt/user/appdata/host-helper
curl -fsSLo compose.yaml https://raw.githubusercontent.com/snds/app-updates/main/host-helper/deploy/compose.yaml
curl -fsSLo config/allow-list.toml https://raw.githubusercontent.com/snds/app-updates/main/host-helper/deploy/allow-list.example.toml
```

Check the project names: `docker compose ls`. If a stack's name isn't its folder's name
(`lockdown`, `gateway`), add `project = "<its name>"` under that stack in
`/mnt/user/appdata/host-helper/config/allow-list.toml`.

**2. One key per server.** Each command makes the helper's copy and writes the server's copy
into that stack's `host-helper.env` (mode 600); the `echo` adds the helper's address.

```sh
docker run --rm \
  -v /mnt/user/appdata/host-helper/config:/config \
  -v /mnt/user/appdata/lockdown:/stack \
  ghcr.io/snds/app-updates-host-helper:main \
  keygen --stack lockdown --keys /config/keys --env-file /stack/host-helper.env
echo 'HOST_HELPER_URL=http://host-helper:7480' >> /mnt/user/appdata/lockdown/host-helper.env

docker run --rm \
  -v /mnt/user/appdata/host-helper/config:/config \
  -v /mnt/user/appdata/vale/apps/gateway:/stack \
  ghcr.io/snds/app-updates-host-helper:main \
  keygen --stack vale-gateway --keys /config/keys --env-file /stack/host-helper.env
echo 'HOST_HELPER_URL=http://host-helper:7480' >> /mnt/user/appdata/vale/apps/gateway/host-helper.env
```

**3. Check the list and start the helper.**

```sh
docker run --rm -v /mnt/user/appdata/host-helper/config:/config:ro \
  ghcr.io/snds/app-updates-host-helper:main check --config /config/allow-list.toml
cd /mnt/user/appdata/host-helper && docker compose up -d && docker compose logs
```

The log ends with `listening on 0.0.0.0:7480`.

**4. Put each server on its helper network.** A `compose.override.yaml` beside each
`compose.yaml` adds the network and `host-helper.env`; compose reads it on its own, and so does
the helper. Then recreate each server once.

```sh
curl -fsSLo /mnt/user/appdata/lockdown/compose.override.yaml \
  https://raw.githubusercontent.com/snds/app-updates/main/host-helper/deploy/stacks/lockdown.compose.override.yaml
cd /mnt/user/appdata/lockdown && docker compose up -d

curl -fsSLo /mnt/user/appdata/vale/apps/gateway/compose.override.yaml \
  https://raw.githubusercontent.com/snds/app-updates/main/host-helper/deploy/stacks/vale-gateway.compose.override.yaml
cd /mnt/user/appdata/vale/apps/gateway && docker compose up -d
```

From now on the helper's stack must be up before these two start (they name its networks).
After a reboot Docker starts all three; if a server ever says `network host-helper-… not found`,
run step 3's `docker compose up -d` first.

**5. Check it works,** calling the helper as each server would, from a throwaway container on
that server's network:

```sh
docker run --rm --network host-helper-lockdown \
  -v /mnt/user/appdata/lockdown/host-helper.env:/env:ro \
  ghcr.io/snds/app-updates-host-helper:main call --env-file /env status

docker run --rm --network host-helper-vale-gateway \
  -v /mnt/user/appdata/vale/apps/gateway/host-helper.env:/env:ro \
  ghcr.io/snds/app-updates-host-helper:main call --env-file /env status
```

Each prints its service as `running`, with its image and digest. `docker compose logs` in
`/mnt/user/appdata/host-helper` shows one line per call.

The admin pages' **Update** and **Reload** buttons use the helper once the servers' next slice
wires them; until then the pages keep showing the command.

### Later

- **Update the helper:** `cd /mnt/user/appdata/host-helper && docker compose pull && docker compose up -d`.
  Don't put the helper on its own allow-list.
- **Revoke a server's key:** delete `/mnt/user/appdata/host-helper/config/keys/<stack>.key` and
  the server's `host-helper.env`, then `docker compose restart` in `/mnt/user/appdata/host-helper`.
  Run step 2 for that stack again to make a new one.
- **Add a stack:** a `[stack.<name>]` block in the allow-list, a network for it and its folder
  in the helper's `compose.yaml` (same path inside as outside), a key (step 2), an override
  (step 4), then `docker compose up -d` for the helper.
- **A socket instead of a network:** `listen = ["unix:/run/host-helper/helper.sock"]` with that
  folder shared to the stack, and `HOST_HELPER_URL=unix:/run/host-helper/helper.sock`. The
  socket is mode 660, so the server's user needs the folder's group. The network is simpler.

## For the servers: the client

A server depends on the client alone (no allow-list or Docker code):

```toml
[dependencies]
snds-app-updates-host-helper = { git = "https://github.com/snds/app-updates", tag = "<tag>", default-features = false }
```

```rust
use host_helper::{Client, ClientError, UpdateRequest};

// HOST_HELPER_URL, HOST_HELPER_STACK and HOST_HELPER_KEY, from host-helper.env.
let helper = Client::from_env()?;          // Err(ClientError::Config) when not installed
let status = helper.status()?;             // each service: state, image, digest, version
let done = helper.update(&UpdateRequest {
    tag: Some("0.1.0-main.20261108093000".into()),   // None: the stack's own tag (main)
    expect_digest: Some("sha256:…".into()),          // from the release manifest, when known
    ..Default::default()
})?;
helper.restart()?;

// A stack with several images: a version tag, and every image's digest by repository.
let done = helper.update(&UpdateRequest {
    tag: Some("0.4.0-main.20261109010000".into()),
    expect_digests: Some(manifest_digests),         // BTreeMap<repository, "sha256:…">
    ..Default::default()
})?;
// done.images: each pulled image; done.before / done.after: each step's exit status and output.
```

While an update runs, `status()` from another thread shows `progress.running` (the operation
and its step: `request`, `pull`, `check`, `before`, `tag`, `recreate`, `after`, or `restart`).
Afterwards `progress.last` says how it ended: `ok`, the `failed_step` and its `error` when it
didn't, and the `after` step's result. It lasts until the helper restarts.

Calls block (an update waits for the pull, up to 20 minutes); an async server runs them on a
blocking thread. Every error's `Display` is a sentence to show on the admin page as is.

## Several images, and steps around an update

A stack can list several images, and a one-shot command to run before and after its update
(Warpcore's block in [`deploy/allow-list.example.toml`](deploy/allow-list.example.toml)):

```toml
[stack.warpcore]
directory = "/mnt/user/appdata/warpcore/src/deploy"
services = ["web", "api", "sandbox"]            # never db or backup
images = ["ghcr.io/snds/warpcore-web", "ghcr.io/snds/warpcore-api", "ghcr.io/snds/warpcore-sandbox"]
before = { service = "backup", command = ["sh", "/backup/backup.sh", "once"] }
after = { service = "sandbox", command = ["node", "dist/selftest.js"], no_deps = true }
```

- **Several images** are one update. The request must name a version tag (not `main`) and
  every image's digest in `expect_digests`, keyed by repository, and nothing else. The helper
  pulls every image at that tag and checks every digest first. If any pull or check fails,
  nothing is tagged, nothing runs, and nothing is recreated. Because the pull is by version
  tag, the `main` tag that compose starts is untouched until every image has passed.
- **`before`** runs `docker compose run --rm --no-TTY --pull never -- <service> <command…>`
  after the checks and before anything changes. If it can't run, or exits with anything but 0,
  the update stops there and nothing is recreated. The reply says its exit status and last
  line.
- **`after`** runs the same way (here with `--no-deps`) once the services are up. Its exit
  status and the last 20 lines of its output (4 KB at most) come back in the reply and in
  `status`. A failed `after` is reported, not undone: the update has already happened, and
  rolling back past a database migration isn't safe.
- A step's service needn't be in `services`: `backup` runs only as the `before` step and is
  never recreated or restarted. A step's command is fixed in the list (up to 16 arguments,
  starting with a program, no control characters).

## The wire, for reference

`POST /v1/status`, `/v1/update` or `/v1/restart`, a JSON body (`{}`, or
`{"tag": …, "expect_digest": …}` or `{"tag": …, "expect_digests": {<repository>: …}}` for
update), and four headers: `x-helper-stack`,
`x-helper-time` (Unix seconds), `x-helper-nonce` (32 hex) and `x-helper-signature`: hex
HMAC-SHA256 with the stack's key over

```text
snds-app-updates-host-helper/1
request
POST
<path>
<stack>
<time>
<nonce>
<hex SHA-256 of the body>
```

A successful reply carries `x-helper-signature` over
`snds-app-updates-host-helper/1\nreply\n<nonce>\n<status>\n<hex SHA-256 of the body>`. A refusal
is `{"error": <code>, "message": <sentence>}` (`tag_not_allowed`, `digest_mismatch`,
`digests_required`, `bad_digests`, `before_failed`, `busy`, `rate_limited`, `stale`,
`replayed`, `bad_signature`, `docker_failed`…). Code:
`src/protocol.rs`.

How an update runs (`src/server.rs`), one step after another, each named in `status`:

1. **request:** check the tag against the list, and the expected digests.
2. **pull:** `docker pull <repository>:<tag>` for each image.
3. **check:** read each image's digest for its repository; refuse if any differs from what the
   request expects.
4. **before** (if the stack has one): `docker compose run --rm …`; stop if it fails.
5. **tag:** if the tag isn't the one `compose.yaml` names, `docker tag <repository>@<digest>
   <repository>:<compose tag>` for each image.
6. **recreate:** `docker compose up --detach --no-deps --no-build --pull never -- <services>`,
   in the stack's folder.
7. **after** (if the stack has one): `docker compose run --rm …`, and report how it ended.

Older clients and servers keep working: the new request field and reply fields are optional
both ways.

## Checks

```sh
cargo fmt --all --check
cargo clippy -p snds-app-updates-host-helper --all-targets -- -D warnings
cargo clippy -p snds-app-updates-host-helper --no-default-features --all-targets -- -D warnings
cargo test -p snds-app-updates-host-helper
cargo test -p snds-app-updates-host-helper --no-default-features
docker build -f host-helper/Dockerfile .
```

The tests run the helper and the client over a real socket against a fake Docker, and the real
`docker` runner against a stand-in program, so no Docker is needed. CI's `host-helper` job runs
them; `host-helper-image.yml` builds the image on every pull request and pushes it on `main`.
