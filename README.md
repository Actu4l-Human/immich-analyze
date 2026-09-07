# Immich Analyze

AI-powered image and audiovisual description generator for Immich

## Overview

Immich Analyze generates searchable descriptions for images and videos in your Immich library. Images can use **Ollama** or an **OpenAI-compatible vision server**; opt-in video analysis uses **self-hosted Qwen3-Omni** to understand visual events, speech, music, and environmental sounds. The included Docker Compose stack runs inference locally and connects to a remote Immich server over its REST API.

The application supports two data access modes:
- **Database mode**: Direct PostgreSQL database access for reading/writing Immich metadata. Note: this mode is planned for removal in a future release (0.5.0 or 0.6.0) since the Immich team does not support direct database access, and schema changes may break compatibility without notice.
- **API mode**: Uses the Immich API (requires `IMMICH_API_URL` + `IMMICH_API_KEY`; supports multiple comma-separated keys for multi-user setups)

## Features

- AI-powered image analysis using Ollama or llama.cpp server with vision-capable models
- Full-duration video analysis in bounded, timestamped segments, including the video's audio track
- Multiple operation modes: batch processing, folder monitoring, or combined mode
- Multi-host support with automatic failover for AI service endpoints
- **Dual data access modes**: Direct PostgreSQL database access OR Immich API integration (database mode is planned for removal in 0.5.0 or 0.6.0)
- Concurrent processing with configurable parallelism
- Configurable retry logic with max retries and delay between attempts
- Internationalization support (English and Russian)
- Docker container support
- Prompt enrichment: optionally enrich AI prompts with asset metadata (EXIF, location, camera info, people with ages, tags, resolution, MIME type) - works **only** in Immich API mode
- Selective description updates: use `--preserve-human` with any overwrite policy to preserve human-written text outside `[AI]...[/AI]` blocks; use `--skip-processed` to skip any saved description before overwrite rules; use `--overwrite-policy missing-ai` to process only assets without existing AI blocks
- Structured logging via `env_logger` (configure with `RUST_LOG` environment variable)
- Wait for Immich to become available on startup (API mode only, configurable timeout)

## Prerequisites

- Immich instance with either:
  - PostgreSQL database access (planned for removal in 0.5.0 or 0.6.0), OR
  - Immich API endpoint with API key
- AI service running a vision-capable model:
  - **Ollama** server (e.g., `qwen3-vl:4b-thinking-q4_K_M`), OR
  - **llama.cpp server** with OpenAI-compatible API endpoint
  - **vLLM-Omni** for audiovisual analysis; the included Compose setup supplies it locally

## Installation

### Local Qwen-Omni with a remote Immich server

The included [`docker-compose.yaml`](docker-compose.yaml) starts two local services:

- **qwen-omni**: built from pinned vLLM-Omni 0.28.0 with the [Intel AutoRound 4-bit Qwen3-Omni checkpoint](https://huggingface.co/Intel/Qwen3-Omni-30B-A3B-Instruct-int4-AutoRound).
- **immich-analyze**: built from this checkout, with FFmpeg/ffprobe included, reading assets and updating descriptions through your remote Immich API.

The local Qwen image includes a focused correction for an upstream INC parser-ownership bug that otherwise selects unquantized expert layers. Its build checks that expert weights remain 4-bit and router gates remain 16-bit. The fix changes metadata ownership only; it does not rewrite the checkpoint or disable audio understanding. See [`deploy/Dockerfile.qwen-omni`](deploy/Dockerfile.qwen-omni) and its checked patch script.

No local Immich, PostgreSQL, Redis, external Docker network, or library mount is needed. Both image and video analysis use the same local model under the served name `qwen-omni`. The `llamacpp` interface setting selects the compatible OpenAI HTTP format; it does not start a separate llama.cpp server.

**Prerequisites**

- NVIDIA GPU access from Linux containers: Docker Desktop with WSL2/GPU support on Windows, or Docker Engine with NVIDIA Container Toolkit on Linux.
- A single-GPU, 32 GB-class deployment target, enough Docker disk space for the image/model cache, and sufficient system memory. Close competing GPU workloads. The supplied configuration quantizes the thinker and omits speech-generation stages; actual capacity still depends on the checkpoint and input.
- Outbound access to Hugging Face for the first model download and to your remote Immich origin. Use trusted HTTPS for Immich; TLS verification remains enabled.
- An Immich API key with asset/search read, original-file download, and description-update permissions. Start with a restricted test user's library: batch/combined mode enumerates all assets visible to the supplied key.

**Configure and start**

Copy `.env.example` to `.env`:

```powershell
Copy-Item .env.example .env
```

On Linux, use `cp .env.example .env`. Set `IMMICH_API_URL` to the remote origin, such as `https://photos.example.com` (do not append `/api`), and set `IMMICH_API_KEY`. Multiple user keys can be comma-separated. `.env` is excluded from Git and the Docker build context.

```bash
docker compose config --quiet
docker pull vllm/vllm-omni:v0.28.0
docker compose up -d --build --wait --wait-timeout 7200
docker compose ps
docker compose logs -f qwen-omni immich-analyze
```

The first startup downloads weights into the `qwen-omni-cache` volume and may take a while. The analyzer waits for model health; subsequent starts reuse the cache. Immich credentials are supplied only to the analyzer, and the optional `HF_TOKEN` only to the model service.

The model API is bound to **127.0.0.1:8091** by default. It is unauthenticated and intended for this machine only; do not publish it on all network interfaces. `QWEN_OMNI_PORT` changes the host port. Containers use `http://qwen-omni:8091`, not localhost or your remote Immich hostname.

Check the model list with `curl.exe http://127.0.0.1:8091/v1/models` on Windows, or `curl` on Linux. It should list `qwen-omni`. A direct PowerShell usage example:

```powershell
$body = @{
    model = "qwen-omni"
    modalities = @("text")
    messages = @(@{ role = "user"; content = "What is 2 + 3? Answer with the number." })
    max_tokens = 64
    stream = $false
} | ConvertTo-Json -Depth 6
$response = Invoke-RestMethod -Uri http://127.0.0.1:8091/v1/chat/completions `
    -Method Post -ContentType application/json -Body $body
$response.choices[0].message.content
```

**Modes and description safety**

The default `IMMICH_ANALYZE_MODE=combined` processes existing eligible assets, then continues detecting new assets concurrently. Set `monitor` to handle only assets added after initial synchronization. For a one-off batch, stop the long-running analyzer first:

```bash
docker compose stop immich-analyze
docker compose run --rm -e IMMICH_ANALYZE_MODE=batch immich-analyze
# Resume the mode configured in .env:
docker compose up -d immich-analyze
```

Existing descriptions are skipped by default (`IMMICH_ANALYZE_OVERWRITE_POLICY=none`), including older poster-frame descriptions. `IMMICH_ANALYZE_SKIP_PROCESSED=true` is the stricter override when you want to skip any saved description, whether AI-wrapped, plain AI, or human-written; cleared descriptions remain eligible. It applies in database/API backends and monitor/combined/batch modes. `missing-ai` processes only assets without an `[AI]` block. This Compose setup preserves human text outside those blocks. Do not run a second batch alongside the active analyzer.

Videos are read from originals, not thumbnails, and processed sequentially in 30-second segments with up to 32 uniformly sampled source frames per segment. The first audio track is analyzed for speech and non-speech content; a missing audio track is valid. Descriptions contain video-relative timestamp ranges. A corrupt/oversized file, failed segment, or truncated model response leaves the existing description unchanged rather than storing a partial result or falling back to a poster frame.

The default original-video limit is 2 GiB (`IMMICH_ANALYZE_VIDEO_MAX_BYTES`), with one active video per process. Temporary media is disk-backed and cleaned after processing. GPU placement/context settings are in [`deploy/qwen-omni.yaml`](deploy/qwen-omni.yaml); those values are literal YAML, not `.env` substitutions. `QWEN_OMNI_GPU_ID` selects the host GPU, which appears as GPU 0 inside the container. Override `QWEN_OMNI_MODEL` only with a compatible Qwen3-Omni checkpoint. Thinker-only deployment disables audio **generation**, not audio **understanding**.

```bash
docker compose down
```

Normal shutdown retains the model cache. Do not use `down -v` unless you intentionally want to remove the downloaded weights.

### Existing Immich stack integration

To integrate Immich Analyze directly into your Immich setup, add the following service to your `docker-compose.yaml` file:

```yaml
services:
  # Optional: Ollama service (you can use external Ollama or llama.cpp server instead)
  # This section is optional - remove it if you want to use external AI service
  ollama:
    image: ollama/ollama:latest
    container_name: ollama
    restart: unless-stopped
    ports:
      - "11434:11434"
    volumes:
      - ./ollama:/root/.ollama
    networks:
      - immich-network
    # Optional: GPU acceleration for NVIDIA cards
    # deploy:
    #   resources:
    #     reservations:
    #       devices:
    #         - driver: nvidia
    #           count: 1
    #           capabilities: [gpu]

  immich-analyze:
    image: ghcr.io/timasoft/immich-analyze:main
    container_name: immich-analyze
    restart: unless-stopped
    volumes:
      # Only required for database mode (to access /data/upload, /data/thumbs)
      - ${UPLOAD_LOCATION}:/data
      - /etc/localtime:/etc/localtime:ro
    env_file:
      - .env
    environment:
      # AI service configuration
      - IMMICH_ANALYZE_INTERFACE=ollama  # or "llamacpp"
      - IMMICH_ANALYZE_HOSTS=http://ollama:11434
      # For llama.cpp server with authentication:
      # - IMMICH_ANALYZE_INTERFACE=llamacpp
      # - IMMICH_ANALYZE_HOSTS=http://llamacpp-server:8080
      # - IMMICH_ANALYZE_API_KEY=your-api-key-here
      # Or use multiple hosts with automatic failover:
      # - IMMICH_ANALYZE_HOSTS=http://primary:11434,http://backup:11434
    depends_on:
      - database
      # Comment the next line if using external AI service
      - ollama
    networks:
      - immich-network

networks:
  immich-network:
    external: true
```

**Important notes about configuration:**

- **Data Access Mode**: You must provide EITHER:
  - Database credentials (`DB_USERNAME`, `DB_PASSWORD`, `DB_DATABASE_NAME`) for direct PostgreSQL access (planned for removal in 0.5.0 or 0.6.0), OR
  - API credentials (`IMMICH_API_URL`, `IMMICH_API_KEY`) for Immich API access
- **Explicit mode override**: Set `IMMICH_ANALYZE_DATA_ACCESS_MODE` to `database` or `immich-api` to bypass auto-detection
- **Volume mounts**: The `/data` volume mount is only required when using **database mode** (to access `upload/` and `thumbs/` directories). When using **API mode**, this volume can be omitted.
- The `ollama` service is **optional** - you can remove it and use an external Ollama or llama.cpp server instead
- Set `IMMICH_ANALYZE_INTERFACE` to `ollama` (default) or `llamacpp` depending on your backend
- If using external service, modify `IMMICH_ANALYZE_HOSTS` to point to your server(s)
- For llama.cpp server, provide `IMMICH_ANALYZE_API_KEY` if authentication is enabled
- After adding the Ollama service, you need to pull the model manually by executing:
  ```bash
  docker exec -it ollama ollama pull qwen3-vl:4b-thinking-q4_K_M
  ```
- For GPU acceleration with NVIDIA cards, uncomment the deploy section and ensure you have NVIDIA Container Toolkit installed

Make sure to:
1. Add the service(s) to your existing `docker-compose.yml` file
2. Ensure the `immich-network` exists or create a new network
3. Add the required environment variables to your `.env` file

After adding the service, run:
```bash
docker-compose up -d immich-analyze
# If using internal Ollama service:
# docker-compose up -d ollama
```

### Nix

If you're using Nix or NixOS, you can build and run the application directly:

**Database mode (planned for removal in 0.5.0 or 0.6.0):**
```bash
nix run github:timasoft/immich-analyze -- --data-access-mode database --immich-root /path/to/immich/data --postgres-url "host=localhost user=your_postgres_user dbname=immich password=your_postgres_password" -c
```

**API mode:**
```bash
IMMICH_API_URL=http://localhost:2283 IMMICH_API_KEY=your_key nix run github:timasoft/immich-analyze -- --data-access-mode immich-api -c
```

### From Source

1. Install Rust toolchain:
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. Install the project:
   ```bash
   cargo install immich-analyze
   ```

3. Run the application:

   **Database mode (planned for removal in 0.5.0 or 0.6.0):**
   ```bash
   immich-analyze --data-access-mode database --immich-root /path/to/immich/data --postgres-url "host=localhost user=your_postgres_user dbname=immich password=your_postgres_password" -c
   ```

   **API mode:**
   ```bash
   IMMICH_API_URL=http://localhost:2283 IMMICH_API_KEY=your_key immich-analyze --data-access-mode immich-api -c
   ```

## Configuration

### Environment Variables (Docker)

#### Data Access Configuration (choose ONE mode)

| Variable | Description | Default | Required For |
|----------|-------------|---------|-------------|
| `IMMICH_ANALYZE_DATA_ACCESS_MODE` | Explicitly set data access mode: `database` or `immich-api`. When unset, mode is auto-detected from other env vars | - | - |
| `DB_USERNAME` | PostgreSQL username | - | Database mode (planned for removal in 0.5.0 or 0.6.0) |
| `DB_PASSWORD` | PostgreSQL password | - | Database mode (planned for removal in 0.5.0 or 0.6.0) |
| `DB_DATABASE_NAME` | PostgreSQL database name | - | Database mode (planned for removal in 0.5.0 or 0.6.0) |
| `DB_HOSTNAME` | PostgreSQL hostname | `database` | Database mode (planned for removal in 0.5.0 or 0.6.0) |
| `DB_PORT` | PostgreSQL port | `5432` | Database mode (planned for removal in 0.5.0 or 0.6.0) |
| `IMMICH_API_URL` | Immich API base URL | - | API mode |
| `IMMICH_API_KEY` | Immich API authentication key(s) (comma-separated for multi-user setups) | - | API mode |

#### AI Service Configuration

| Variable | Description | Default |
|----------|-------------|---------|
| `IMMICH_ANALYZE_INTERFACE` | AI service interface type (`ollama` or `llamacpp`) | `ollama` |
| `IMMICH_ANALYZE_HOSTS` | Comma-separated AI service host URLs | `http://localhost:11434` |
| `IMMICH_ANALYZE_API_KEY` | API key for llama.cpp server authentication | *(none)* |
| `IMMICH_ANALYZE_MODEL_NAME` | Model name for image analysis | `qwen3-vl:4b-thinking-q4_K_M` |
| `IMMICH_ANALYZE_PROMPT` | Prompt for generating image descriptions | *See below* |
| `IMMICH_ANALYZE_ENRICH_PROMPT` | Enable prompt enrichment with asset metadata (API mode only) | `false` |
| `IMMICH_ANALYZE_API_POLL_INTERVAL` | Poll interval for API mode in seconds | `10` |

#### Video Analysis Configuration

These are general application defaults. The supplied Compose file enables video analysis and overrides both model names/hosts to the local `qwen-omni` service.

| Variable | Description | Default |
|----------|-------------|---------|
| `IMMICH_ANALYZE_VIDEO_HOSTS` | Comma-separated OpenAI-compatible Omni server origins; nonempty enables original-video analysis. Do not append `/v1`. | *(disabled)* |
| `IMMICH_ANALYZE_VIDEO_MODEL_NAME` | Model served by the video hosts | `Qwen/Qwen3-Omni-30B-A3B-Instruct` |
| `IMMICH_ANALYZE_VIDEO_API_KEY` | Optional bearer key, read directly from the environment | *(none)* |
| `IMMICH_ANALYZE_VIDEO_PROMPT` | Video description prompt covering visuals, speech, music, and environmental sounds | *Built-in audiovisual prompt* |
| `IMMICH_ANALYZE_VIDEO_MAX_CONCURRENT` | Maximum active videos, shared by batch and monitoring | `1` |
| `IMMICH_ANALYZE_VIDEO_MAX_BYTES` | Maximum original-video size; files are rejected, not truncated | `2147483648` |

Without video hosts, existing preview-only behavior remains available. Standalone binaries need `ffmpeg` and `ffprobe` on PATH when video analysis is enabled; Docker and Nix packages include them. Missing original-download permission is an error, not a visual-only fallback.

#### Application Settings

| Variable | Description | Default |
|----------|-------------|---------|
| `IMMICH_ANALYZE_MODE` | Operating mode: `monitor`, `combined`, or `batch` | `combined` |
| `IMMICH_ANALYZE_OVERWRITE_EXISTING` | If true, overwrite existing descriptions (alias for `--overwrite-policy all`) | `false` |
| `IMMICH_ANALYZE_OVERWRITE_POLICY` | Overwrite policy: `none` (skip any with description), `all` (process everything), `missing-ai` (process only if no `[AI]...[/AI]` block). Overrides `IMMICH_ANALYZE_OVERWRITE_EXISTING` and is overridden by `IMMICH_ANALYZE_SKIP_PROCESSED` | `none` |
| `IMMICH_ANALYZE_SKIP_PROCESSED` | If true, skip any asset with a nonempty saved description before overwrite policy is applied; cleared descriptions remain eligible. Overrides every overwrite setting | `false` |
| `IMMICH_ANALYZE_PRESERVE_HUMAN` | If true, preserve human text outside `[AI]...[/AI]` blocks by only replacing the AI block. Incompatible with `--disable-ai-wrapper` | `false` |
| `IMMICH_ANALYZE_LANG` | Interface language for the application (en, ru) | `en` |
| `IMMICH_ANALYZE_MAX_CONCURRENT` | Max concurrent AI requests | `4` |
| `IMMICH_ANALYZE_UNAVAILABLE_DURATION` | Host availability check interval in seconds | `60` |
| `IMMICH_ANALYZE_TIMEOUT` | AI request timeout in seconds | `300` |
| `IMMICH_ANALYZE_DISABLE_AI_WRAPPER` | If true, disable `[AI]...[/AI]` wrapper, storing description as plain text. Incompatible with `--preserve-human`. When combined with `missing-ai` overwrite policy, every asset will be re-analyzed (no `[AI]` tag to detect) | `false` |
| `IMMICH_ANALYZE_NO_FINAL_OUTPUT` | If true, disable final output with analysis results and statistics after batch processing | `false` |
| `IMMICH_ANALYZE_MAX_RETRIES` | Maximum retry attempts (0 = infinite) | `0` |
| `IMMICH_ANALYZE_RETRY_DELAY_SECONDS` | Delay between retry cycles in seconds | `5` |
| `IMMICH_ANALYZE_HEALTH_PORT` | Port for health check HTTP server (0 to disable) | `3000` |
| `IMMICH_ANALYZE_WAIT_FOR_IMMICH` | Wait for Immich to become available on startup (API mode only) | `true` |
| `IMMICH_ANALYZE_WAIT_TIMEOUT` | Maximum time in seconds to wait for Immich (0 = no limit) | `120` |
| `IMMICH_ANALYZE_WAIT_RETRY_INTERVAL` | Interval in seconds between retry attempts when waiting | `5` |
| `RUST_LOG` | Logging level (`error`, `warn`, `info`, `debug`, `trace`) | `info` |

> **Default prompt**: `Create a detailed description for the image for proper image search functionality. In the response, provide only the description without introductory words. Also specify the image format (Wallpaper, Screenshot, Drawing, City photo, Selfie, etc.). The format must be correct. If in doubt, name the most likely option and don't think too long.`

> **Backwards Compatibility**: The deprecated `IMMICH_ANALYZE_OLLAMA_HOSTS` variable is still supported and will be automatically mapped to `IMMICH_ANALYZE_HOSTS` when `IMMICH_ANALYZE_INTERFACE=ollama`.

### Command Line Arguments

```txt
Usage: immich-analyze [OPTIONS]

Options:
  -m, --monitor
          Enable folder monitoring mode
  -c, --combined
          Enable combined mode: process existing images then monitor for new ones
  -o, --overwrite-existing
          Overwrite existing entries in database (process all files regardless of existing descriptions) (same as --overwrite-policy all)
  -O, --overwrite-policy <OVERWRITE_POLICY>
          Overwrite policy [default: none]: none (skip any with description), all (process everything), missing-ai (process only if no [AI]...[/AI] block). Takes precedence over --overwrite-existing [possible values: none, all, missing-ai]
      --skip-processed
          Skip any asset with a nonempty saved description before overwrite policy is applied; cleared descriptions remain eligible (takes precedence over all overwrite settings)
  -p, --preserve-human
          When overwriting or adding, preserve human-entered text by only replacing the [AI]...[/AI] block
      --immich-root <IMMICH_ROOT>
          Path to Immich root directory (containing upload/, thumbs/ folders) [default: /var/lib/immich]
      --postgres-url <POSTGRES_URL>
          `PostgreSQL` connection string (used only in database mode) [default: "host=localhost user=postgres dbname=immich password=your_password"]
  -d, --data-access-mode <DATA_ACCESS_MODE>
          Data access mode: database (direct `PostgreSQL`) or api (Immich REST API) [default: database] [possible values: database, immich-api]
      --immich-api-url <IMMICH_API_URL>
          Immich API base URL (required when using api access mode) [env: IMMICH_API_URL=]
      --immich-api-keys <IMMICH_API_KEYS>
          Immich API authentication key(s) (required when using api access mode). Provide multiple keys comma-separated for multi-user setups [env: IMMICH_API_KEY]
      --api-poll-interval <API_POLL_INTERVAL>
          API poll interval in seconds (for Immich API mode) [default: 10]
      --model-name <MODEL_NAME>
          Ollama model name for image analysis [default: qwen3-vl:4b-thinking-q4_K_M]
      --interface <INTERFACE>
          AI service interface type [default: ollama] [possible values: ollama, llamacpp]
      --hosts <HOSTS>
          Host URLs (Ollama or llama.cpp server) [default: http://localhost:11434]
      --video-hosts <VIDEO_HOSTS>
          Host URLs for video analysis (OpenAI-compatible server)
      --video-model-name <VIDEO_MODEL_NAME>
          Model name for video analysis [default: Qwen/Qwen3-Omni-30B-A3B-Instruct]
      --video-api-key <VIDEO_API_KEY>
          API key for video host authentication [env: IMMICH_ANALYZE_VIDEO_API_KEY]
      --video-prompt <VIDEO_PROMPT>
          Prompt for generating video description
      --video-max-concurrent <VIDEO_MAX_CONCURRENT>
          Maximum number of concurrent video requests [default: 1]
      --video-max-bytes <VIDEO_MAX_BYTES>
          Maximum original video size in bytes [default: 2147483648]
      --api-key <API_KEY>
          API key for authentication (llama.cpp server) [env: IMMICH_ANALYZE_API_KEY]
      --max-concurrent <MAX_CONCURRENT>
          Maximum number of concurrent requests [default: 4]
      --unavailable-duration <UNAVAILABLE_DURATION>
          Host availability check interval in seconds [default: 60]
      --timeout <TIMEOUT>
          HTTP request timeout in seconds [default: 300]
      --file-write-timeout <FILE_WRITE_TIMEOUT>
          File write timeout in seconds [default: 30]
      --file-check-interval <FILE_CHECK_INTERVAL>
          File stability check interval in milliseconds [default: 500]
      --event-cooldown <EVENT_COOLDOWN>
          Minimum time between processing identical events in seconds [default: 2]
      --prompt <PROMPT>
          Prompt for generating image description [default: "Create a detailed description for the image for proper image search functionality. In the response, provide only the description without introductory words. Also specify the image format (Wallpaper, Screenshot, Drawing, City photo, Selfie, etc.). The format must be correct. If in doubt, name the most likely option and don't think too long."]
      --lang <LANG>
          Interface language (ru, en) [default: ""]
      --max-retries <MAX_RETRIES>
          Maximum number of retry attempts (0 = infinite) [default: 0]
      --retry-delay-seconds <RETRY_DELAY_SECONDS>
          Delay between retry cycles in seconds (fixed) [default: 5]
      --enrich-prompt
          Enable prompt enrichment with asset metadata (date, location, camera info)
      --disable-ai-wrapper
          Disable [AI]...[/AI] wrapper around AI-generated description
      --no-final-output
          Disable final output with analysis results and statistics after batch processing
      --no-wait-for-immich
          Disable waiting for Immich to become available on startup (API mode only)
      --wait-timeout <WAIT_TIMEOUT>
          Maximum time in seconds to wait for Immich to become available (0 = no limit) [default: 120]
      --wait-retry-interval <WAIT_RETRY_INTERVAL>
          Interval in seconds between retry attempts when waiting for Immich [default: 5]
      --health-port <HEALTH_PORT>
          Port for health check HTTP server (0 to disable) [default: 3000]
  -h, --help
          Print help (see more with '--help')
  -V, --version
          Print version
```

> **Note**: `IMMICH_API_URL` and `IMMICH_API_KEY` are read from environment variables by clap when using `--data-access-mode immich-api` - no need to pass them as command-line arguments. `IMMICH_API_KEY` supports multiple comma-separated keys for multi-user setups.

## Usage Examples

### Database Mode (planned for removal in 0.5.0 or 0.6.0)

**Basic Batch Processing with Ollama**
```bash
immich-analyze \
  --data-access-mode database \
  --postgres-url "host=localhost user=postgres dbname=immich password=password" \
  --interface ollama \
  --hosts "http://ollama-server:11434"
```

**Basic Batch Processing with llama.cpp Server**
```bash
immich-analyze \
  --data-access-mode database \
  --postgres-url "host=localhost user=postgres dbname=immich password=password" \
  --interface llamacpp \
  --hosts "http://llamacpp-server:8080"
```

**Batch Processing with Human Text Preservation (Overwrite All)**
```bash
immich-analyze \
  --data-access-mode database \
  --postgres-url "host=localhost user=postgres dbname=immich password=password" \
  --interface ollama \
  --hosts "http://ollama-server:11434" \
  --overwrite-existing \
  --preserve-human
```

**Selective Processing: Add AI Blocks to Human-Only Descriptions**
```bash
immich-analyze \
  --data-access-mode database \
  --postgres-url "host=localhost user=postgres dbname=immich password=password" \
  --interface ollama \
  --hosts "http://ollama-server:11434" \
  --overwrite-policy missing-ai \
  --preserve-human
```

**Monitor Mode (Watch for new images)**
```bash
immich-analyze \
  --monitor \
  --data-access-mode database \
  --postgres-url "host=localhost user=postgres dbname=immich password=password" \
  --interface ollama \
  --hosts "http://ollama:11434,http://ollama-backup:11434"
```

**Monitor Mode with Infinite Retries**
```bash
immich-analyze \
  --monitor \
  --data-access-mode database \
  --postgres-url "host=localhost user=postgres dbname=immich password=password" \
  --interface ollama \
  --hosts "http://ollama:11434" \
  --retry-delay-seconds 10
```

**Batch Processing with Limited Retries**
```bash
immich-analyze \
  --data-access-mode database \
  --postgres-url "host=localhost user=postgres dbname=immich password=password" \
  --interface ollama \
  --hosts "http://ollama-server:11434" \
  --max-retries 3 \
  --retry-delay-seconds 10
```

### API Mode

**Basic Batch Processing via Immich API**
```bash
IMMICH_API_URL=http://immich:2283 \
IMMICH_API_KEY=your_api_key \
immich-analyze \
  --data-access-mode immich-api \
  --interface ollama \
  --hosts "http://ollama-server:11434"
```

**Multi-User Batch Processing (Multiple API Keys)**
```bash
IMMICH_API_URL=http://immich:2283 \
IMMICH_API_KEY=user1_key,user2_key,user3_key \
immich-analyze \
  --data-access-mode immich-api \
  --interface ollama \
  --hosts "http://ollama-server:11434"
```

**Combined Mode with API Access and llama.cpp**
```bash
IMMICH_API_URL=http://immich:2283 \
IMMICH_API_KEY=your_api_key \
IMMICH_ANALYZE_API_KEY=your-llamacpp-api-key \
immich-analyze \
  --combined \
  --data-access-mode immich-api \
  --interface llamacpp \
  --hosts "http://llamacpp-primary:8080,http://llamacpp-secondary:8080" \
  --api-poll-interval 30
```

**Monitor Mode with Infinite Retries**
```bash
IMMICH_API_URL=http://immich:2283 \
IMMICH_API_KEY=your_api_key \
immich-analyze \
  --data-access-mode immich-api \
  --interface ollama \
  --hosts "http://ollama:11434" \
  --monitor
```

**Batch Processing with Prompt Enrichment**
```bash
IMMICH_API_URL=http://immich:2283 \
IMMICH_API_KEY=your_api_key \
immich-analyze \
  --data-access-mode immich-api \
  --interface ollama \
  --hosts "http://ollama-server:11434" \
  --enrich-prompt
```

**Batch Processing with Limited Retries**
```bash
IMMICH_API_URL=http://immich:2283 \
IMMICH_API_KEY=your_api_key \
immich-analyze \
  --data-access-mode immich-api \
  --interface llamacpp \
  --hosts "http://llamacpp-server:8080" \
  --max-retries 5 \
  --retry-delay-seconds 15
```

**Batch Processing Without Final Results Output**
```bash
immich-analyze \
  --data-access-mode database \
  --postgres-url "host=localhost user=postgres dbname=immich password=password" \
  --interface ollama \
  --hosts "http://ollama-server:11434" \
  --no-final-output
```

### Enable Debug Logging
```bash
RUST_LOG=debug immich-analyze --combined --data-access-mode database --postgres-url "..." --interface ollama
```

## Model Recommendations

### For Ollama:
- `qwen3-vl:4b-thinking-q4_K_M` (Default) - Good balance of speed and accuracy
- `qwen3-vl:30b-a3b-thinking-q4_K_M` - Higher accuracy for complex images
- `qwen3-vl:2b-instruct-q4_K_M` - Faster processing for simpler descriptions

### For llama.cpp Server:
- Any GGUF vision model served via llama.cpp's OpenAI-compatible API
- Recommended: `qwen3-vl-4b-instruct-q4_k_m.gguf` or similar quantized variants

Install Ollama models using:
```bash
ollama pull qwen3-vl:4b-thinking-q4_K_M
```

## Architecture

The application integrates with your Immich instance by analyzing preview images and storing generated descriptions. It supports multiple operation modes:

- **Batch Mode**: Process all existing images in your library
- **Monitor Mode**: Automatically process new images as they're added to Immich
- **Combined Mode**: Process existing images in background while simultaneously monitoring for new additions

### Data Access Modes

#### Database Mode (planned for removal in 0.5.0 or 0.6.0)
- Direct access to Immich PostgreSQL database for reading/writing metadata
- Direct filesystem access to `thumbs/` directory for image analysis
- Uses filesystem events for monitoring new images
- Requires `--immich-root` and `--postgres-url` configuration

#### API Mode
- Uses Immich REST API for all data operations
- No direct database or filesystem access required
- Polls Immich API for new assets at configurable interval (`--api-poll-interval`)
- Requires `IMMICH_API_URL` and `IMMICH_API_KEY` environment variables (supports multiple comma-separated keys)

### Core Features
- Automatic retry logic with multiple AI service hosts and automatic failover
  - Configurable maximum retry attempts (`--max-retries`, 0 = infinite)
  - Configurable delay between retry cycles (`--retry-delay-seconds`)
  - Smart error classification: only retryable errors (5xx HTTP, timeouts, host unavailable) trigger retries
  - Non-retryable errors (invalid UUID, empty response, JSON parsing) fail immediately
- Host unavailability tracking with configurable recovery duration
- File stability checks (database mode) to ensure images are fully written before processing
- Event cooldown (database mode) to prevent duplicate processing of rapid filesystem events
- Prompt enrichment: optionally enrich AI prompts with asset metadata (EXIF metadata, location, camera info, recognized people with ages, tags, resolution, MIME type) via the Immich API for more detailed descriptions
- Selective description preservation: when using `--preserve-human`, only the `[AI]...[/AI]` block in the description is replaced, preserving any human-written text outside this block. If no `[AI]...[/AI]` block exists, the AI-generated block is appended to the existing description
- Processing controls: use `--skip-processed` (or `IMMICH_ANALYZE_SKIP_PROCESSED=true`) to skip any saved description before overwrite policy is considered; this is the safest override and cleared descriptions remain eligible. Use `--overwrite-policy missing-ai` when you only want to skip assets with an existing `[AI]...[/AI]` block (processes human-only and empty descriptions). Use `--overwrite-policy all` to process everything, or `none` for the default conservative skip-any-description behavior
- Structured logging via `env_logger` for easier debugging and monitoring

## Troubleshooting

### Enable verbose logging
Set the `RUST_LOG` environment variable to see detailed logs:
```bash
RUST_LOG=debug immich-analyze --combined ...
```

### Check AI service status
- For Ollama: `systemctl status ollama` or `curl http://localhost:11434/api/tags`
- For llama.cpp: `curl http://localhost:8080/health`

### API Mode Issues
- Verify `IMMICH_API_URL` is reachable: `curl $IMMICH_API_URL/api/server/ping`
- Verify API key has sufficient permissions in Immich admin panel
- Check Immich server logs for authentication errors

## TODO:
- [x] Add llama.cpp support
- [x] Add support for Immich API
- [x] ~~Add waiting list~~ Add retry logic
- [x] Rename ignore-existing option/variable to overwrite-existing
- [x] Add support for multiple Immich API keys
- [ ] Add JWT support
- [ ] Add NixOS service module
- [x] Add video and audio analysis
