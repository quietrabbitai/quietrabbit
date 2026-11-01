# Privacy Filter model: attribution

Quiet Rabbit's outbound privacy check runs OpenAI's Privacy Filter, a PII
token-classification model. Quiet Rabbit does not bundle it in the app
installer. It downloads the file below at first login and verifies it against
a size and SHA-256 compiled into the app (items.id=678, decisions.id=864).

| | |
|---|---|
| Model | `openai/privacy-filter` (OpenAI), licensed Apache-2.0 |
| GGUF conversion and Q8_0 quantization | `LocalAI-io/privacy-filter-GGUF` on Hugging Face, licensed Apache-2.0 |
| Pinned revision | `935d86882f4b5dc6e0eef05aae48ae5ba0a82a7a` |
| File | `privacy-filter-q8.gguf` |
| Size | 1,637,572,192 bytes |
| SHA-256 | `80efc1803eda7c095a79741d2008c07e2e0a57b01bac8825fbeb448fd097998c` |
| Runtime | `localai-org/privacy-filter.cpp` (the C library Quiet Rabbit links) |

Quiet Rabbit redistributes the file byte-for-byte as published at the pinned
revision. It does not modify it, and the release workflow refuses to publish
anything whose size or SHA-256 differs from the values above.

The full Apache License, Version 2.0 text is in `LICENSE-Apache-2.0.txt`
beside this file. Source model card: https://huggingface.co/openai/privacy-filter
GGUF packaging card: https://huggingface.co/LocalAI-io/privacy-filter-GGUF
