---
name: purchase-order
description: Draft a purchase order from approved reorder quantities and follow up late receipts — create the draft only, park confirmation for approval, draft (never send) the supplier email, and hand the confirmed order to Finn. Use when the user asks to order stock, raise or draft a PO, confirm a PO, chase a supplier, or check late deliveries.
license: MIT
author: aivory
version: 0.1.0
category: operations
tags: [purchasing, purchase-order, supplier, tenant-scoped]
---

# Purchase order

## Draft

1. Use the quantities the owner agreed (from a stock check or their own). Do not change them.
2. One draft PO per supplier. Create it as a draft in the ERP (Odoo `purchase.order`, ERPNext `MAKE_PURCHASE_ORDER`). Creating the draft is itself parked for approval: say `awaiting approval` until it is approved, then give the PO number.
3. Never set or edit a price. Use the supplier price list as the ERP fills it; if there is none, write `[NEED: unit price]` and stop.
4. Show: supplier, lines (product, quantity, unit), total and currency, expected receipt date (order date plus lead time), payment terms or `[NEED: terms]`.

## Confirm

Confirming (`button_confirm` / submit) is a separate approval. Ask the owner, state exactly what will be confirmed, and wait. After it is approved and done, send Finn a `purchase` packet (see ops-rules) with the PO number, supplier, total, terms and who approved it and when.

## Supplier email

Draft only (Gmail or Outlook draft). Say `drafted`, show the text, and say that sending needs approval. Keep it factual: PO number, items, quantities, requested delivery date. No price negotiation, no commitments beyond the PO.

## Late receipts

List open POs whose expected date has passed, with the PO number and days late. For each, offer a follow-up email draft. If a late receipt blocks open sales orders, send an `alert` packet to Lex and Teo with the affected order numbers; if you cannot read the affected orders, write `impact: [NEED: affected sales orders]`.

## Do not

- Validate a receipt or transfer yourself.
- Cancel a document, change a price, or send mail without approval.
- Say `sent`, `confirmed` or `done` for anything still waiting.
