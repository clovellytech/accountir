# Accountir Ingest API

Base URL: `http://localhost:9876`

The ingest API lets external systems (POS, inventory management) push business events to accountir. Accountir translates them into double-entry journal entries. The external system describes what happened in business terms — accountir handles the accounting.

## Setup: Account Mappings

Before using the ingest endpoints, you must configure which accounts in your chart of accounts correspond to each business concept. Without these mappings, ingest requests return `412 Precondition Failed`.

### Mapping Keys

| Key | Description | Account Type |
|-----|-------------|--------------|
| `pos_cash` | Cash register / cash on hand | Asset |
| `pos_square` | Square receivables | Asset |
| `pos_revenue` | Sales revenue | Revenue |
| `cogs` | Cost of Goods Sold | Expense |
| `inventory` | Inventory | Asset |
| `sales_tax_payable` | Sales tax liability | Liability |
| `accounts_payable` | Accounts payable | Liability |
| `inventory_adjustment` | Inventory adjustment expense | Expense |

You only need to configure mappings for the endpoints you use. For example, if you only use cash sales (no Square), you don't need `pos_square`.

### `GET /api/ingest/mappings`

Returns all configured mappings.

**Response:**

```json
{
  "mappings": [
    {
      "key": "pos_cash",
      "account_id": "a1b2c3d4",
      "account_name": "Cash on Hand"
    },
    {
      "key": "inventory",
      "account_id": "e5f6g7h8",
      "account_name": "Inventory"
    }
  ]
}
```

### `PUT /api/ingest/mappings`

Set or update account mappings. You can send one or many at a time. Each `account_id` must reference an existing, active account.

**Request:**

```json
{
  "mappings": [
    { "key": "pos_cash", "account_id": "a1b2c3d4" },
    { "key": "pos_revenue", "account_id": "e5f6g7h8" },
    { "key": "cogs", "account_id": "i9j0k1l2" },
    { "key": "inventory", "account_id": "m3n4o5p6" },
    { "key": "sales_tax_payable", "account_id": "q7r8s9t0" }
  ]
}
```

**Response:**

```json
{
  "success": true,
  "updated": 5
}
```

**Errors:**

- `400` — Unknown mapping key, or account not found / inactive
- `503` — No database open in accountir

---

## Endpoints

### `POST /api/ingest/sale`

Record a POS sale. Creates a balanced journal entry with revenue, payment, COGS, inventory reduction, and optionally sales tax.

**Required mappings:** `pos_cash` or `pos_square` (depending on `payment_method`), `pos_revenue`, `cogs`, `inventory`. Also `sales_tax_payable` if `tax_collected_cents > 0`.

**Request:**

```json
{
  "date": "2026-04-24",
  "reference": "POS-20260424-001",
  "memo": "Walk-in sale",
  "items": [
    {
      "name": "Continental GP5000 700c",
      "qty": 2,
      "unit_price_cents": 6999,
      "unit_cost_cents": 3500
    },
    {
      "name": "Tube 700x25c",
      "qty": 2,
      "unit_price_cents": 899,
      "unit_cost_cents": 350
    }
  ],
  "payment_method": "square",
  "tax_collected_cents": 1264
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `date` | string | yes | `YYYY-MM-DD` |
| `reference` | string | no | Unique ID for idempotency. Duplicate POSTs with the same reference return the existing entry. |
| `memo` | string | no | Defaults to item summary |
| `items` | array | yes | At least one item |
| `items[].name` | string | yes | Product name |
| `items[].qty` | integer | yes | Quantity sold |
| `items[].unit_price_cents` | integer | yes | Retail price per unit in cents |
| `items[].unit_cost_cents` | integer | yes | Cost (for COGS/inventory) per unit in cents |
| `payment_method` | string | yes | `"cash"` or `"square"` |
| `tax_collected_cents` | integer | no | Sales tax collected, in cents. Defaults to 0. |

**Journal entry created:**

```
DR  Cash/Square     (revenue + tax)     "Payment received"
CR  Revenue         (sum of qty*price)  "Sales revenue"
CR  Sales Tax       (tax)               "Sales tax collected"
DR  COGS            (sum of qty*cost)   "Cost of goods sold"
CR  Inventory       (sum of qty*cost)   "Inventory reduction"
```

**Response:**

```json
{
  "success": true,
  "entry_id": "f47ac10b-58cc-4372-a567-0e02b2c3d479",
  "total_revenue_cents": 15796,
  "total_cogs_cents": 7700
}
```

---

### `POST /api/ingest/purchase-order`

Record inventory received from a supplier. Creates a journal entry debiting inventory and crediting either cash or accounts payable.

**Required mappings:** `inventory`, and either `pos_cash` (if `payment` is `"cash"`) or `accounts_payable` (if `"on_credit"`).

**Request:**

```json
{
  "date": "2026-04-20",
  "reference": "PO-2026-042",
  "memo": "Spring restock",
  "supplier": "Shimano",
  "items": [
    {
      "name": "CN-HG701 Chain",
      "qty": 20,
      "unit_cost_cents": 2499
    },
    {
      "name": "CS-R7000 Cassette",
      "qty": 10,
      "unit_cost_cents": 4299
    }
  ],
  "payment": "on_credit"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `date` | string | yes | `YYYY-MM-DD` |
| `reference` | string | no | Unique ID for idempotency |
| `memo` | string | no | Defaults to supplier + item summary |
| `supplier` | string | no | Supplier name (used in default memo) |
| `items` | array | yes | At least one item |
| `items[].name` | string | yes | Product name |
| `items[].qty` | integer | yes | Quantity received |
| `items[].unit_cost_cents` | integer | yes | Cost per unit in cents |
| `payment` | string | yes | `"cash"` or `"on_credit"` |

**Journal entry created:**

```
DR  Inventory           (total cost)  "Inventory received"
CR  Cash / Accts Pay    (total cost)  "Cash payment" or "Accounts payable"
```

**Response:**

```json
{
  "success": true,
  "entry_id": "b2c3d479-58cc-4372-a567-0e02f47ac10b",
  "total_cost_cents": 92970
}
```

---

### `POST /api/ingest/inventory-adjustment`

Record inventory corrections — shrinkage, damage, or found inventory. Creates a journal entry between the inventory account and an adjustment expense account.

**Required mappings:** `inventory`, `inventory_adjustment`.

**Request:**

```json
{
  "date": "2026-04-24",
  "reference": "ADJ-2026-003",
  "items": [
    {
      "name": "Continental GP5000 700c",
      "qty_delta": -2,
      "unit_cost_cents": 3500,
      "reason": "damaged"
    },
    {
      "name": "Tube 700x25c",
      "qty_delta": -1,
      "unit_cost_cents": 350,
      "reason": "shrinkage"
    }
  ]
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `date` | string | yes | `YYYY-MM-DD` |
| `reference` | string | no | Unique ID for idempotency |
| `memo` | string | no | Defaults to item + reason summary |
| `items` | array | yes | At least one item |
| `items[].name` | string | yes | Product name |
| `items[].qty_delta` | integer | yes | Change in quantity. Negative = loss, positive = found. |
| `items[].unit_cost_cents` | integer | yes | Cost per unit for valuation |
| `items[].reason` | string | no | e.g. `"shrinkage"`, `"damaged"`, `"count correction"` |

The net adjustment across all items must be non-zero.

**Journal entry created (net negative — loss):**

```
DR  Inventory Adjustment Expense    |net|   "Inventory adjustment expense"
CR  Inventory                       |net|   "Inventory reduction"
```

**Journal entry created (net positive — found):**

```
DR  Inventory                       net     "Inventory increase"
CR  Inventory Adjustment Expense    net     "Inventory adjustment credit"
```

**Response:**

```json
{
  "success": true,
  "entry_id": "d479b2c3-4372-58cc-a567-f47ac10b0e02",
  "net_adjustment_cents": -7350
}
```

---

## Error Responses

All errors return:

```json
{
  "success": false,
  "error": "Human-readable error message"
}
```

| Status | Meaning |
|--------|---------|
| `400` | Bad request — invalid date, empty items, unknown mapping key |
| `412` | Missing required account mappings |
| `422` | Entry creation failed — account not found, period closed, etc. |
| `503` | No database open in accountir |

## Idempotency

If you include a `reference` field, accountir checks for an existing non-voided journal entry with that reference before creating a new one. If found, the existing entry is returned with `200 OK`. This makes it safe to retry requests on network failure.

## Finding Account IDs

To configure mappings, you need account IDs from your chart of accounts. You can find these via:

```
GET /accounts/banks
```

This returns all accounts with their IDs, names, and types. Use the `id` field when setting up mappings.
