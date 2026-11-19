# agentrouter-mcp

Высокопроизводительный, модель-агностичный Model Context Protocol (MCP) сервер на чистом Rust для проведения глубокого код-ревью через **AgentRouter** (OpenAI-совместимый API).

---

## Возможности

1. **Модель-агностичность (Zero Hardcode)**:
   - Сервер не привязан к конкретной модели в коде. Любая модель платформы (например, `gpt-6.1-sol`, `claude-3-7-sonnet`, `deepseek-r1` и др.) может быть выбрана на лету или задана через аргумент `--default-model`.
   - Инструмент `list_models` динамически запрашивает актуальный список доступных моделей с сервера AgentRouter.
2. **Безопасность первого класса (Zero Leaks)**:
   - Никаких переменных окружения (`AGENTROUTER_API_KEY`) и plaintext-файлов на диске.
   - Токен читается исключительно по требованию из системного хранилища через `secret-tool lookup service agentrouter` в эфемерную память процесса.
3. **Стандарты Rust Workflow & Cranelift**:
   - Сборка по Сценарию B (`rust-workflow`): тулчейн `nightly`, быстрый кодогенератор `cranelift`, линковщик `mold`.
   - Поддержка глобального кэша `/mnt/cache/build_dir` и `sccache`.
   - Исчерпывающий `match`, строгая типизация, отсутствие неконтролируемых `.clone()` и `unwrap_or_default()`.
4. **Спецификация MCP 2024-11-05**:
   - Полноценная поддержка stdio-транспорта на базе `rmcp 3.11.0`.
   - Логирование строго в `stderr`, протокол JSON-RPC — в `stdout`.

---

## Экспортируемые MCP-инструменты

* **`code_review`**:
  Основной инструмент для глубокого код-ревью.
  - `title` (строка, обязательный): заголовок изменений/PR.
  - `diff` (строка, обязательный): git diff или код для анализа.
  - `context` (строка, опциональный): проектные правила, инварианты, архитектура, ошибки компилятора.
  - `model` (строка, опциональный): идентификатор модели на AgentRouter (при отсутствии используется `--default-model`).
* **`sol_code_review`**:
  Псевдоним (alias) для `code_review` для обратной совместимости.
* **`list_models`**:
  Возвращает актуальный список всех доступных моделей с эндпоинта `/v1/models` AgentRouter.
* **`agentrouter_health`**:
  Диагностический инструмент. Проверяет наличие ключа в `secret-tool` и проверяет связь с API (без расхода токенов на генерацию).

---

## Безопасность и получение ключей (Zero Hardcode & Zero Leaks)

Сервер **не содержит захардкоженных команд или утилит** для извлечения ключей. Вместо этого команда для получения токена вынесена во внешнюю конфигурацию (`.env` или аргумент командной строки):

1. **Конфигурация через `.env` (рекомендуется)**:
   В файле `.env` (добавлен в `.gitignore`) указывается shell-команда получения ключа:
   ```bash
   # Команда извлечения токена из системного хранилища (выполняется on-demand)
   AGENTROUTER_KEY_CMD="secret-tool lookup service agentrouter"

   # Опционально: модель по умолчанию
   AGENTROUTER_DEFAULT_MODEL="gpt-6.1-sol"
   ```
   *Сам API-ключ в `.env` не хранится!* В файле задаётся лишь команда доступа к защищённому хранилищу (`secret-tool`, `pass`, `rbw`, `op`, `keepassxc-cli` и т.д.).

2. **Конфигурация через CLI**:
   Команду можно передать напрямую при запуске сервера:
   ```bash
   agentrouter-mcp --key-cmd "secret-tool lookup service agentrouter"
   ```

3. **Эфемерность в ОЗУ**:
   Токен извлекается строго перед отправкой запроса к AgentRouter и существует исключительно в оперативной памяти процесса. В логи выводится только замаскированная форма (`sk-a...1234`).

---

## Параметры командной строки

```text
Usage: agentrouter-mcp [OPTIONS]

Options:
  -k, --key-cmd <COMMAND>      Shell command to retrieve API key on-demand
                               (default: secret-tool lookup service agentrouter)
  -m, --default-model <MODEL>  Default model when not specified in tool calls
                               (default: gpt-6.1-sol)
  -u, --base-url <URL>        Base URL for AgentRouter API
                               (default: https://agentrouter.org/v1)
  -t, --timeout <SECS>        HTTP timeout in seconds (default: 300)
      --env-file <PATH>       Path to .env configuration file
  -h, --help                  Print this help message
```

---

## Сборка и разработка

```bash
# Проверка типов через Cranelift (-j 2 для 2-ядерного CPU)
cargo check -j 2

# Сборка релизного бинарника
cargo build --release -j 2
```

Релизный бинарник сохраняется в `/mnt/cache/build_dir/release/agentrouter-mcp`.
