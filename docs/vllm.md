# Running kern against vLLM

Any leg (`[reason]`, `[answer]`, `[embed]`) can point at an OpenAI-compat
server. The `/v1` suffix on the configured URL is the switch: bare local URLs
get Ollama's native `/api/*` path, an explicit `/v1` forces the compat path
(`/v1/chat/completions`, `/v1/embeddings`) regardless of host. No other config
keys involved.

```toml
[reason]
url = "http://localhost:8100/v1"
model = "Qwen/Qwen2.5-7B-Instruct-AWQ"   # vLLM's served model name, not an Ollama tag
```

Compat-path behavior:
- `seed`/`temperature` (eval pins) are forwarded; vLLM honors both.
- `num_ctx`/`keep_alive`/`num_gpu` are native-only — VRAM budgeting is the
  server's job (`--gpu-memory-utilization`), and the serving-mode CPU pin for
  the reason model does not apply.
- One model per vLLM instance; legs route independently, so embed can stay on
  Ollama while reason runs on vLLM.

## WSL2 launch (verified e2e 2026-07-17, vLLM 0.25.1, RTX 4060 8 GB)

```sh
VLLM_WSL2_ENABLE_PIN_MEMORY=1 VLLM_USE_FLASHINFER_SAMPLER=0 \
  .venv/bin/vllm serve Qwen/Qwen2.5-1.5B-Instruct --port 8100 \
  --max-model-len 8192 --gpu-memory-utilization 0.55 --enforce-eager
```

Each flag clears a WSL2-specific startup failure:
- `VLLM_WSL2_ENABLE_PIN_MEMORY=1` — pinned memory is off by default under
  WSL2; without it startup dies with `RuntimeError: UVA is not available`.
- `--enforce-eager` — torch inductor JIT-compiles with `nvcc`; no CUDA
  toolkit in the WSL distro (`cuda_home='/usr/local/cuda' doesn't exist`).
- `VLLM_USE_FLASHINFER_SAMPLER=0` — the flashinfer sampler JIT-compiles with
  `nvcc` too, independently of eager mode.

Free VRAM first if Windows-side Ollama holds the serving models
(`powershell.exe ollama stop <model>`); vLLM preallocates its whole budget at
startup and the two will not coexist on 8 GB.
