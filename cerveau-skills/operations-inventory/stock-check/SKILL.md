---
name: stock-check
description: Run the daily or weekly stock check — read on-hand and incoming stock from the ERP, read demand and supplier lead times, call reorder_suggestion once for the whole list, and report stock-outs, items below the reorder point and the suggested quantities with their inputs. Use when the user asks for a stock check, low-stock list, what to reorder, inventory status, or stock-out risk.
license: MIT
author: aivory
version: 0.1.0
category: operations
tags: [inventory, reorder, stock, tenant-scoped]
---

# Stock check

Target: correct and sourced in few steps. Do not wander.

## Steps

1. **Scope.** Ask one question only if the scope is unclear (which warehouse, which product group). Otherwise use all stocked products.
2. **Read stock.** Odoo: `stock.quant` summed per product (and warehouse) for on hand; `purchase.order.line` on open orders for on order. ERPNext: `GET_STOCK_BALANCE`. Name the system and the date.
3. **Read demand.** Units sold or consumed over a window you state (default 60 days when the history exists): Odoo `sale.order.line` / stock moves with `odoo_read_group`; ERPNext sales invoice items. Record how many days the window covers. Fewer than 14 days is directional.
4. **Read lead time and rules.** Supplier lead time from the supplier record or recent PO history (order date to receipt date); MOQ and pack size from the supplier info. If a product has no lead time you can find, leave the item out of the call and list it under what you could not determine, asking the owner for it.
5. **One call.** `reorder_suggestion` with every item in one list (up to 50; split bigger lists). Do not call it per product.
6. **Report** in the layout below. Plain text, short.

## Layout

```
Stock check: <business or warehouse>
<date>, basis: <system>, demand window <n> days

The one thing
<one sentence: how many items are out or below their reorder point>

Out of stock
- <product>: on hand 0, <n> on order, daily demand <x> [source]

Reorder now
- <product>: on hand <a>, reorder point <rp> (demand <d>/day x lead <l> days + safety <s>), suggest <qty> [source]

Covered by open orders (check they are not late)
- <product>: on hand <a>, <b> arriving <date>

Do these first
1. <action, who approves it>

What I could not determine
- <missing lead times, short demand history, assumptions applied, products left out>
```

Show the inputs on every line you suggest an order for. Tell the user which defaults were applied (`assumptions`).

## Do not

- Round a quantity yourself or change what the tool returned.
- Suggest an order for an item that came back with errors.
- Create a purchase order from a stock check. Offer the draft instead (see purchase-order).
- Raise an alert on a base smaller than 5 sales records without saying it is directional.
