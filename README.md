# akira · forja

Pipeline agéntico de features sobre Claude Code. Un solo binario en Rust (`forja`) que conecta
cualquier repo y convierte cada feature en un PR listo para revisar: los agentes planean,
implementan, documentan y prueban; un gate determinístico decide si el código es válido, y tú
decides si entra a `main`.

- **Dos puntos humanos:** HITL #1 aprueba el plan; HITL #2 aprueba el PR.
- **Los LLM crean, lo determinístico decide:** formato, lint, pruebas y doc-drift son comandos con salida binaria.
- **Repo-agnóstico:** todo lo del repo vive en `.forja/config.yaml`; los adaptadores YAML traducen el contrato por stack.
- **Guardrails en código:** `forja hook` es el hook PreToolUse de cada agente; además, el orquestador revierte lo que un rol no podía tocar.

## Instalación

```bash
cargo install --path .     # deja `forja` en ~/.cargo/bin
forja setup                # crea ~/.forja y copia los adaptadores para editarlos
forja doctor               # git, gh, claude, docker y herramientas de cada adaptador
```

Requiere el CLI `claude` autenticado. `gh` es opcional: sin él, forja deja la rama lista en
su worktree (estado `ready`) en lugar de abrir el PR.

## Uso rápido

```bash
forja                                   # TUI: Repos · Features · Plan · En vivo · Feature map · PRs

forja connect ~/dev/yuzuko              # descubre (read-only), propone config.yaml, registra
forja gate --repo yuzuko                # gate determinístico: format_check, lint, test por componente
forja bootstrap --repo yuzuko           # rama forja/bootstrap: make ci, forja-ci, docs/, hooks (PR si pasa el gate)

forja feature new --repo yuzuko --title "Cancelar pedidos" --description "…"   # intake → features/FEAT-001.yaml
forja feature plan FEAT-001 --repo yuzuko        # planificador (solo lectura) → FEAT-001.plan.md
forja feature approve FEAT-001 --repo yuzuko     # HITL #1   (o: reject --comment "…" --replan)
forja feature run FEAT-001 --repo yuzuko         # implement → test → quality → docs → gate → PR
forja pr-sync --repo yuzuko                      # HITL #2: merge / cierre / cambios pedidos desde GitHub
forja feature status FEAT-001 --repo yuzuko      # historial auditable y eventos
```

`FORJA_HOME=/otra/ruta` aísla todo el estado global (útil para probar).

## Qué hace cada etapa

| Etapa | Implementado | Cómo |
|---|---|---|
| Repos y adaptadores | ✅ | Registro SQLite en `~/.forja/registry.db`; descubrimiento de Go, Python, TS/JS y Rust; borrador de config con huecos |
| Bootstrap | ✅ parcial | `Makefile` (`make ci` → `.forja/ci.sh`), `.github/workflows/forja-ci.yml`, docs, `features/`, `CHANGES/`, hooks en `.claude/settings.json`, sección en `CLAUDE.md`. Falta generar Dockerfile y compose de CI |
| 1. Intake + plan (HITL #1) | ✅ | Intake con IA sin herramientas; el planificador solo tiene Read/Grep/Glob y declara un bloque `impact` |
| 2. Coordinador | ✅ | Worktree y rama `feat/FEAT-NNN-…`; ciclo implement → test → gate de código → quality (veredicto JSON), con hasta `max_retries.code` vueltas y un commit convencional en cada vuelta |
| 3. Documentación | ✅ | El agente solo ve el diff y escribe en `docs/` y `CHANGES/`; FEATURE_MAP generado de forma determinística; discrepancias entre el plan y el diff |
| 4. Gate | ✅ 4a parcial + doc-drift | Pasos del adaptador por componente. Docker build, gitleaks, trivy y el ambiente efímero quedan pendientes |
| 5. QA de caja negra | ⏳ | Se omite de forma explícita (queda registrado en el PR y en el feature map) |
| 6. PR (HITL #2) | ✅ | Rebase, push solo de la rama, `gh pr create` con un cuerpo generado; `pr-sync` lee el merge, el cierre o los cambios pedidos |
| TUI | ✅ | Seis pantallas; los trabajos largos corren como procesos `forja …` en segundo plano |

## Guardrails

Cada agente corre como `claude -p` con `--tools` limitado a su rol, `--permission-mode dontAsk`,
`--max-budget-usd` igual al presupuesto restante y este hook:

```json
{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"forja hook --role implement --root <worktree>"}]}]}}
```

| Rol | Escribe en |
|---|---|
| `planner`, `quality`, `qa` | nada (solo lectura) |
| `implement` | `agents.allowed_paths`, sin archivos de prueba |
| `test` | solo archivos de prueba (`test_globs` del adaptador) |
| `docs` | `docs/**` y `CHANGES/**` |
| `human` | el repo (lo instala el bootstrap en `.claude/settings.json`) |

Reglas para todos los roles: no se leen ni escriben archivos sensibles (`.env*`, llaves, `.pass`,
credenciales). No se permite `git push`, `git reset --hard`, `rm -rf` ni cambios a la rama principal.
La red solo llega a la lista blanca. Los agentes no tocan `.forja/`, `.claude/` ni `.github/`, ni
manejan git: los commits los hace el orquestador. Cada bloqueo queda en
`~/.forja/logs/guardrails.jsonl`.

Después de cada nodo, el orquestador revisa `git status` contra la misma política y revierte lo que
se haya escapado (por ejemplo, una escritura hecha por Bash). Esa violación regresa al agente como
feedback.

## Estructura

```
src/
  main.rs        CLI (clap)
  tui.rs         TUI (ratatui)
  discovery.rs   descubrimiento read-only y borrador de config
  config.rs      modelo y validación de .forja/config.yaml
  adapters.rs    contrato por stack (adapters/*.yaml embebidos + ~/.forja/adapters)
  gate.rs        gate determinístico
  guard.rs       guardrails: hook PreToolUse + política por rol
  agent.rs       runner de `claude -p --output-format stream-json`
  pipeline.rs    intake, plan, coordinador, docs, gate, PR, HITL
  bootstrap.rs   PR de bootstrap
  git.rs db.rs feature.rs paths.rs
adapters/        go, python, typescript, rust
```

```bash
cargo test     # incluye pruebas de la política de guardrails
```
