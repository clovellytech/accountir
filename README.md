# Accountir

An event-sourced double-entry accounting system with a terminal UI, CLI, and browser extension for automatic bank transaction synchronization.

## Features

- **Event Sourcing**: All changes are recorded as immutable events with Merkle tree verification for audit integrity
- **Double-Entry Accounting**: Full support for assets, liabilities, equity, revenue, and expense accounts
- **Terminal UI**: Interactive interface for managing accounts, journal entries, and generating reports
- **CLI**: Programmatic access to all accounting operations
- **Browser Extension**: Automate bank transaction downloads with a learn-and-replay recipe system
- **GnuCash Import**: Import existing data from GnuCash files
- **Financial Reports**: Trial balance, balance sheet, and income statement generation

## Prerequisites

### Rust Application

- [Rust](https://rustup.rs/) (stable toolchain)

### Browser Extension

- [Node.js](https://nodejs.org/) and npm
- [web-ext](https://github.com/mozilla/web-ext) for development: `npm install -g web-ext`
- [ImageMagick](https://imagemagick.org/) for building icons (optional)

## Installation

### Building the Rust Application

```bash
# Clone the repository
git clone <repository-url>
cd accountir

# Build in release mode
cargo build --release

# The binary will be at target/release/accountir
```

### Setting Up the Browser Extension

```bash
cd extension

# Install dependencies
npm install

# Build icons (requires ImageMagick)
npm run build

# Run in Firefox for development
web-ext run

# For Chrome/Edge: Load unpacked extension from the extension/ directory
```

## Usage

### Terminal User Interface

The TUI provides an interactive experience for managing your accounting:

```bash
# Launch with default database (accountir.db)
cargo run -- tui

# Launch with a specific database
cargo run -- tui -d mycompany.db
```

### CLI Commands

**Initialize a new database:**
```bash
cargo run -- init
```

**Account management:**
```bash
# Create an account
cargo run -- account create -t asset -n "1000" --name "Checking Account"

# List all accounts
cargo run -- account list

# Show account balance
cargo run -- account balance -a <account_id>

# Show account ledger
cargo run -- account ledger -a <account_id>
```

**Journal entries:**
```bash
# Post a journal entry
cargo run -- entry post -d "2025-02-20" -m "Monthly sales" \
  -l <debit_account_id>:10000 \
  -l <credit_account_id>:-10000

# List entries
cargo run -- entry list
```

**Reports:**
```bash
# Trial balance
cargo run -- report trial-balance

# Balance sheet
cargo run -- report balance-sheet -a "2025-02-20"

# Income statement
cargo run -- report income-statement --start "2025-01-01" --end "2025-02-28"
```

**Merkle tree verification:**
```bash
cargo run -- merkle verify
```

### HTTP Server (for Browser Extension)

Start the server to enable communication with the browser extension:

```bash
cargo run -- serve
```

The server runs on `http://localhost:9876` and provides:
- `GET /health` - Health check endpoint
- `POST /import/bank-csv` - Bank CSV import endpoint

### GnuCash Import

Import existing GnuCash data:

```bash
# Import to default database
cargo run -- import-gnucash ~/Downloads/myfile.gnucash

# Import to a specific database
cargo run -- import-gnucash ~/Downloads/myfile.gnucash -o imported.db
```

Supports both gzip-compressed (`.gnucash`) and plain XML formats.

## Browser Extension

The browser extension automates bank transaction downloads using a learn-and-replay approach:

1. **Learn Mode**: Navigate to your bank and record the steps to download transactions
2. **Replay Mode**: Automatically replay recorded steps to download new transactions
3. **CSV Interception**: Automatically captures downloaded CSV files and sends them to Accountir

No credentials are stored in recipes - you enter your password during replay.

### Extension Setup

1. Start the Accountir server: `cargo run -- serve`
2. Load the extension in your browser
3. Click the extension icon and select "Add Bank"
4. Follow the prompts to record a recipe for your bank
5. Use "Sync" to replay the recipe and import transactions

See `extension/README.md` for detailed extension documentation.

## Development

### Quick Start

Use the development script to set up a tmux environment with all components:

```bash
./dev.sh
```

This opens:
- Editor (nvim)
- Claude CLI
- Cargo run
- Browser extension (web-ext)

### Running Tests

```bash
cargo test
```

### Project Structure

```
accountir/
├── src/
│   ├── main.rs           # CLI entry point
│   ├── lib.rs            # Library exports
│   ├── domain/           # Domain models (accounts, entries, money)
│   ├── events/           # Event types and validation
│   ├── store/            # Event store, projections, Merkle tree
│   ├── tui/              # Terminal UI application
│   ├── commands/         # CLI command handlers
│   ├── queries/          # Read queries and reports
│   ├── server/           # HTTP server for extension
│   └── gnucash/          # GnuCash import parser
├── extension/            # Browser extension
│   ├── src/
│   │   ├── background/   # Service worker
│   │   ├── popup/        # Extension popup UI
│   │   ├── content/      # Content scripts
│   │   └── recorder/     # Recipe recording logic
│   └── manifest.json
├── migrations/           # SQLite schema migrations
├── examples/             # Example files
└── dev.sh                # Development environment script
```

## Architecture

Accountir uses an **event sourcing** architecture:

- All changes are recorded as immutable events in SQLite
- A Merkle tree provides cryptographic verification of the ledger
- Projections denormalize events into queryable state for fast reads
- The system maintains full audit history with no data mutations

## License

[Add your license here]
