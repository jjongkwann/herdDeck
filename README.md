# HerdDeck

Herdr가 감지한 Claude Code, Codex CLI, Gemini CLI 세션을 한곳에서 확인하고 대화하는 Tauri 데스크톱 앱입니다.

## 요구 사항

- macOS 또는 Unix 계열 환경
- Node.js 및 Rust
- 실행 중인 Herdr와 Unix socket (`~/.config/herdr/herdr.sock`)
- 선택 사항: `claude`, `codex`, `gemini` CLI

## 개발 실행

```sh
npm install
npm run app
```

기본 개발 포트는 `14200`입니다. 충돌하면 `RL_PORT=14201 npm run app`처럼 바꿀 수 있습니다.

## 검증과 빌드

```sh
npm run build
cargo test --manifest-path src-tauri/Cargo.toml
npm run tauri build -- --debug --bundles app
```

Claude 대화 타임라인은 `~/.claude/projects` 아래의 JSONL 트랜스크립트만 읽습니다. 메시지 전송은 Herdr의 로컬 socket을 사용합니다.
