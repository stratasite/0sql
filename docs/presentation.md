# How a value is shown: format, display_type, hidden

A semantic model carries three properties no plan reads: `format`,
`display_type` and `hidden`. They are for the application in front of the
engine. A client formats a value first, then renders the formatted text
by its display type, and leaves a hidden column out of what it shows.
The engine checks all three where they are written, keeps each in one
shape, and sends them out with every result column. It never applies one:
rows come back raw in JSON, CSV and Arrow, as the warehouse returned them.

`crates/semantic/src/presentation.rs` holds the rules, and the loader,
the spec and the planner all call it.

## Where they are written

**On a field**, in the model's YAML. That is the default for every query
that projects the field.

```yaml
- type: measure
  name: Revenue
  data_type: decimal
  format: currency:2
- type: dimension
  name: Account Name
  data_type: string
  display_type: { type: link, href: "https://crm.example.com/{{value}}" }
- type: dimension
  name: Account Id
  data_type: integer
  hidden: true
```

**On a projection**, in the spec. That overrides the field's for one
query, and it is the only place a calculation, which has no field, gets a
format:

```json
{ "projections": [
  { "field": "Account Id", "hidden": true },
  { "field": "Account Name",
    "display_type": { "type": "link", "href": "https://crm.example.com/accounts/{{Account Id}}" } },
  { "field": "Revenue", "format": "raw" },
  { "calculation": true, "alias": "Margin", "sql": "...", "format": "percent:1" }
] }
```

The order is the projection's own, then what a decorator makes of the
value, then the field's. On a projection, `format: raw` clears the
field's format, `display_type: none` clears its display type, and
`display_type: default` or a null says nothing and keeps the field's.
Strata echoes `default` back on every projection, which is why it means
"keep".

`hidden` on a field leaves it out of `fields` listings and search, and the
planner still plans it. `hidden` on a projection marks the column as
carried but not shown, as a column another column's template reads is.
The two are separate: a hidden field that a query projects is not a
hidden column.

## Format

Written either as a shortcut string or as a mapping with a `type`:

| type | options | shortcut |
|---|---|---|
| `number` | `precision` (0–20), `abbreviate`, `delimiter`, `separator`, `units` | `number:2:abbreviate` |
| `currency` | the same, plus `unit` (default `$`); precision defaults to 2 | `currency:2:€` |
| `percent` | `precision`, default 2; the value is a fraction, 0.12 is 12% | `percent:1` |
| `date`, `datetime` | `pattern`, strftime with `%{quarter}` | `date:short`, `date:long`, `date:iso`, `date:%b %Y` |
| `template` | `template`, text with `{{...}}` slots | `template:https://x.io/{{value}}` |
| `raw` | none: no format | `raw` |

An unknown type or option is refused at deploy time and in the spec,
naming what the type takes. Strata's `html` type is gone: write
`type: template` with the markup and `display_type: html` to render it.
Strata's `javascript` type is not supported: a model does not carry code
for its clients to run.

Each declarative type maps onto an Excel or Sheets number format. A
template and every display type are a UI's concern and have no export
form.

## Display type

A name, or a mapping with a `type` and that type's options:

| type | options | data types |
|---|---|---|
| `link` | `href`, `label` (templates); without `href` the formatted value is the href | any |
| `email` | | string |
| `phone` | | any |
| `image` | `alt` (template) | string |
| `html` | | string |
| `markdown` | | string |
| `transcript` | | string |
| `json` | | string |
| `x-...` | anything | any |

`url` and `phone_number`, strata's names, are read as `link` and `phone`.
A type that starts `x-` belongs to the client, so an application can add
a renderer without an engine release. It passes through unchecked. Any
other unknown type is refused.

Format runs first, then display. For a link whose text is not its
target, the format makes the text and the display type's `href` builds
the target:

```yaml
format: number:0
display_type: { type: link, href: "https://crm.example.com/accounts/{{value}}" }
```

## Templates

A template is text with `{{name}}` slots. On a **field** the only slot is
`{{value}}`, the value being shown. A model does not know which other
columns a query will carry, so a field's template that names any other
slot is refused when the project loads.

On a **projection**, a slot may also name another column of the same
query, as the result names it: its alias, or the name a decorator gives
it, such as `Month(Date)`. The slot is matched without regard to case and
sent spelled as the column is, so a client looks it up in the row
exactly. A slot that names no column of the query is refused, and the
refusal lists the query's columns. To carry a column only for a template,
project it with `hidden: true`. The engine never adds one silently.

`{{value}}` always means the value. A column named "value" cannot be read
by a slot. Values are substituted as they are, so a client rendering
`html` has to sanitize.

## Decorators

A decorator that changes what the value is changes how it is shown:

| decorator | effect |
|---|---|
| truncate | keeps the field's format and display, and sends `grain` so a client can pick a pattern (a month is not shown as a day) |
| extract | drops both, and the data type, and sends `extract`: a weekday is not a date |
| contribute, temporal `percent_change`, window `percent_rank`/`cume_dist` | become `{type: percent}`, no display type |
| window `rank`, `dense_rank`, `row_number`, `ntile`, `count` | drop both: a rank is not money |
| other windows, customize, plain temporal | keep the field's |

A projection's own format and display type still win over all of this.

## Where they come out

- **`sql` and `explain`** answer with `columns`, one per column of the
  final SELECT and in its order: `name`, `kind`, `field_uid` (absent for a
  calculation), `calculation`, `data_type`, `grain`, `extract`, `format`,
  `display_type`, `hidden`. A column from a visitor's rewrite, such as a
  share of the total computed as a calculation, is reported as the
  projection the caller wrote.
- **The run manifest's `columns`** carry the same values, taken from the
  plan by position. A column the transport names differently at that
  position falls back to the model lookup by name.
- **`fields`** includes a field's `format` and `display_type` when it
  has them.
- **MCP** keeps these lean: `fields` sends the format's type alone, so an
  agent knows 0.12 is 12% or that an amount is money, and no display
  type. `sql` and `explain` send `columns` only for the columns that have
  a format, as `{name, format: <type>}`, and no `columns` key when none
  do.

## Snapshot

`Field.display_type` is new and defaults when absent, so snapshot
format 2 still reads older snapshots and needs no new number.
`Field.format` keeps its JSON shape: the loader now writes the checked
shape, and a snapshot written before it loads as it was.
