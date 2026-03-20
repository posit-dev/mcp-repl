VENV ?= .venv
PYTHON ?= $(VENV)/bin/python
PIP ?= $(VENV)/bin/pip
INSPECT ?= $(VENV)/bin/inspect

TASK ?= eval/inspect_ai/latent_relationships/task.py
LOG_DIR ?= eval/inspect_ai/latent_relationships/logs
SEED ?= 1
ROWS ?= 800
COLS ?= 40
MODEL ?= gpt-5.2-codex
REASONING_EFFORT ?= low
RUNS ?= 1

.PHONY: venv eval-baseline eval-console eval-loop analyze \
	snap-test snap-review snap-accept snap-reject snap-pending snap-check snap-force-update

venv:
	python3 -m venv $(VENV)
	$(PIP) install -U pip
	@echo "NOTE: eval harness deps are optional and not required for mcp-repl itself."
	@echo "Install inspect_ai however you prefer (pip, uv, or editable checkout), then:"
	@echo "  $(PIP) install pyyaml pandas"
	$(PIP) install pyyaml pandas

eval-baseline:
	$(INSPECT) eval $(TASK) -T seed=$(SEED) -T n_rows=$(ROWS) -T n_cols=$(COLS) -T use_console=false -T codex_model=$(MODEL) -T codex_reasoning_effort=$(REASONING_EFFORT) --model none --no-log-realtime --log-dir $(LOG_DIR) --metadata condition=baseline --metadata seed=$(SEED)

eval-console:
	$(INSPECT) eval $(TASK) -T seed=$(SEED) -T n_rows=$(ROWS) -T n_cols=$(COLS) -T use_console=true -T codex_model=$(MODEL) -T codex_reasoning_effort=$(REASONING_EFFORT) --model none --no-log-realtime --log-dir $(LOG_DIR) --metadata condition=console --metadata seed=$(SEED)

eval-loop:
	$(PYTHON) eval/inspect_ai/latent_relationships/run_eval.py --both --runs=$(RUNS) --seed $(SEED) --rows $(ROWS) --cols $(COLS) --model $(MODEL) --reasoning-effort $(REASONING_EFFORT) --log-dir $(LOG_DIR)

analyze:
	$(PYTHON) eval/inspect_ai/latent_relationships/analyze_results.py --log-dir $(LOG_DIR)

snap-test:
	cargo insta test

snap-review:
	cargo insta review

snap-accept:
	cargo insta accept

snap-reject:
	cargo insta reject

snap-pending:
	cargo insta pending-snapshots

snap-check:
	cargo insta test --check --unreferenced=reject

snap-force-update:
	cargo insta test --force-update-snapshots --accept
