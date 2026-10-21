# The expression language, rewritten

Implemented 2026-10-20 in `crates/shorthand` (parser, lowering, tests)
and released as 0.6.1; the request field is still `expr`. The shorthand
this replaced read each comma item by keyword search and covered four
shapes of the spec. This is a small formal grammar that reaches every attribute of the
query spec, written to be cheap in tokens for a model and to read like
SQL for a person at a prompt. Nothing here is backward compatible with
the shorthand, by design.

## Goals, in order

1. **Parity.** Every attribute the planner reads has a spelling. The
   few the spec only carries, and the display hints, reach it through
   one escape hatch, so parity is complete by construction.
2. **Token economy.** Bare multi-word names, no quoting of the common
   case, no clause keywords for the common shapes, commas as the only
   separator.
3. **SQL-shaped.** Prefix functions, `extract(month, date)`, `between a
   and b`, `in (...)`, `order by`, single-quoted strings. Someone who
   reads SQL reads this.
4. **A grammar, not a cascade.** One PEG, so an error says what was
   expected where, and `describe` can echo how each item was read.
5. **Escape, never guess.** Brackets or double quotes make a name a
   field whatever it says; single quotes make a value a string whatever
   it holds; braces pass spec JSON through untouched.
6. **One spelling per thing.** No synonyms, no short forms beside long
   ones. A reader learns each construct once.

## The shape

```
item category, month(order date), store net paid, yoy_pct(store net paid) as Growth desc,
order date between 2024-01-01 and 2024-12-31, item category top 5 by store net paid,
include (customer id, catalog net paid > 0), limit 100
```

Items are separated by commas or newlines. Each item is read by its
leading structure:

| Starts with | Item |
|---|---|
| `limit`, `via` | a query setting |
| `include`, `exclude`, `expand` | a segment |
| `{` | raw spec JSON merged into the query |
| `(` | a filter group |
| `calc(` | a calculation |
| a name, then a predicate | a filter |
| a name, or a function around one | a projection |

The same example as a spec is about 620 bytes against 230, and roughly
five times the tokens.

## Lexical rules

Fourteen reserved words end a bare name wherever they appear (bracket
or quote a name that holds one):

```
and or not in is between top contains starts ends keyword as asc desc
```

Five item-leading words are keywords only at the start of an item:
`limit`, `via`, `include`, `exclude`, `expand`, and only when what the
keyword needs follows, a number or a parenthesis. `include tax` is a
projection of a field with that name.

Context words are keywords only where the grammar expects them and are
plain words everywhere else: `by` after `top N` and inside a function's
parentheses; `order`, only when the next word is `by`; `null` after
`is`; `with` after `starts`, `ends` and an `expand (...)` group; `for`,
`left`, `inner` and `dedupe` in a segment's tail; `neq`, `lt`, `none`
after `dedupe`; the type names after a top-level comma in `calc(...)`;
the function names and extract parts, which are only read before `(`
or inside `extract(`. So `order date`, `created by` and `null count`
are bare names at the top level, and `order by` is the clause.

Keywords and names are matched without regard to case. A quote opens a
field or a string only at the start of a token, so `Men's` is a word,
`'Men''s'` is a string and `"Men's Wear"` is a field. A `-` is a letter,
so `2024-01-01`, `28d` and the uid `store-net-paid` are single words;
subtraction exists only inside `calc(...)`.

## Grammar

The grammar is a parsing expression grammar (PEG), in Ford's notation:
`<-` defines, `/` is ordered choice (the first alternative that matches
wins, so `Setting / Segment / ... / Projection` is the item table above
made exact), `*` `+` `?` repeat, `!e` succeeds only if `e` does not
match here and consumes nothing, `&e` likewise but requires a match,
`.` is any character, `'x'i` is a literal matched without regard to
case, and `[...]` is a character class. The parser is the grammar read
top down with no backtracking past an ordered choice, which is what a
recursive-descent parser does.

```
# ---------------------------------------------------------------- structure

Query       <- _ Item (Sep Item)* Sep? _ EOF
Item        <- Raw / (Setting / Segment / Filter / Projection) (WS Raw)? ItemEnd
Sep         <- WS (',' / NL) _                          # a comma or a newline between items
ItemEnd     <- &(WS (',' / NL / ')' / EOF))              # or one of the error guards listed below

Setting     <- LIMIT SP Int
             / VIA SP Names
Names       <- '(' _ Field (_ ',' _ Field)* _ ')' / Field

Raw         <- '{' JsonBody '}'                          # balanced braces; the text must parse as a JSON object
JsonBody    <- (JString / '{' JsonBody '}' / !'}' .)*
JString     <- '"' ('\\' . / !'"' .)* '"'

# -------------------------------------------------------------- projections

Projection  <- Expr (SP Modifier)*                       # one of each; the lowering rejects a repeat
Modifier    <- AS SP Field / Dir
Dir         <- ASC / DESC

Expr        <- Calc / Extract / Custom / Deco / Ref
Ref         <- Field Kind?
Kind        <- ('@d'i / '@m'i) WB                        # no space before it
Calc        <- CALC WS '(' Sql (',' _ Type)? _ ')'
Extract     <- EXTRACT WS '(' _ Part _ ',' _ Expr _ ')'
Custom      <- CUSTOM WS '(' _ String _ ',' _ Expr _ ')'
Deco        <- FName WS '(' _ Inner Args _ ')'
Inner       <- Calc / Extract / Custom / Deco / PField Kind?    # a bare name here stops at `by` and `order by`
Args        <- (_ ',' _ Int)?                            # rows, offset or buckets, by function
               (_ BY SP PField (_ ',' _ PField)*)?       # partition: projected dimensions
               (_ ORDER SP BY SP Order (_ ',' _ Order)*)?
Order       <- PField (SP Dir)?

# The literal groups below rely on their order: a longer spelling before
# its prefix (share_filtered before share, month_name before month,
# year_month before year, date_time before date), because the word
# boundary sits outside the group and PEG does not retry inside it. A
# hand-written parser reads one word and looks it up instead.
FName       <- Truncate / Temporal / Share / Window
Truncate    <- ('raw' / 'millisecond' / 'second' / 'minute' / 'hour' / 'day' / 'week'
              / 'month' / 'quarter' / 'year')i WB
Temporal    <- ('yoy' / 'qoq' / 'mom' / 'wow' / 'dod')i ('_pct'i)? WB
Share       <- ('share_filtered' / 'share')i WB
Window      <- (('running_' / 'moving_')i ('sum' / 'avg' / 'min' / 'max' / 'count')i
              / ('dense_rank' / 'rank' / 'row_number' / 'percent_rank' / 'cume_dist'
                / 'ntile' / 'lag' / 'lead' / 'first_value' / 'last_value')i) WB
Part        <- ('minute' / 'hour' / 'day_of_month' / 'day_of_week' / 'day_of_year'
              / 'week_of_year' / 'month_name' / 'month' / 'quarter' / 'year_month'
              / 'year' / 'day_name')i WB
Type        <- ('string' / 'integer' / 'decimal' / 'date_time' / 'date' / 'boolean' / 'bigint')i WB

# The SQL of a calc runs to the parenthesis that closes it. Parentheses,
# strings, quoted identifiers and bracket references are balanced, and a
# comma at the top level ends it; the grammar then demands a Type.
Sql         <- SqlTop+
SqlTop      <- String / DString / BracketRef / '(' SqlIn* ')' / !(')' / ',') .
SqlIn       <- String / DString / BracketRef / '(' SqlIn* ')' / !')' .
BracketRef  <- '[' (!']' .)+ ']'

# ------------------------------------------------------------------ filters

Filter      <- Conj (_ OR SP Conj)*                     # and/or may start a continuation line
Conj        <- Pred (_ AND SP Pred)*
Pred        <- '(' _ Filter _ ')'
             / Ref WS Test
Test        <- CmpOp WS Value                           # a value never comes from the next line
             / (NOT SP)? IN WS List
             / BETWEEN SP Value SP AND SP Value
             / (NOT SP)? (CONTAINS / STARTS SP WITH / ENDS SP WITH) SP Value
             / IS SP (NOT SP)? NULL
             / TOP SP Int (SP BY SP Field)?
             / KEYWORD WS List
CmpOp       <- ('>=' / '<=' / '!=' / '<>' / '=' / '>' / '<') ![=<>!]
List        <- '(' _ ListValue (_ ',' _ ListValue)* (_ ',')? _ ')'

# A value is a string, or plain text to the next separator. In a list
# the text runs to the next comma or parenthesis, so `and` and `or` are
# plain words there. A quote character can only open a string, so an
# unclosed one, or a double-quoted value, is an error and never text.
Value       <- String / !['"] Text
Text        <- (!ValueStop .)+
ValueStop   <- WS (',' / ')' / '{' / NL / EOF) / SP (AND / OR)
ListValue   <- String / !['"] (!(WS (',' / ')' / NL)) .)+

# ----------------------------------------------------------------- segments

Segment     <- (INCLUDE / EXCLUDE) _ Group (SP Tail)*
             / EXPAND _ Group _ WITH _ '(' _ Carried (_ ',' _ Carried)* _ ')' (SP Tail)*
Group       <- '(' _ SItem (Sep SItem)* Sep? _ ')'
SItem       <- Filter / Field '@m'i / Field                 # a lone name is a key; with @m, a measure
Carried     <- Field (SP AS SP Field)?
Tail        <- AS SP TField
             / FOR SP TNames
             / LEFT / INNER
             / DEDUPE SP (NEQ / LT / NONE)
TNames      <- '(' _ Field (_ ',' _ Field)* _ ')' / TField

# ------------------------------------------------------------------- fields

# A field is bracketed, double-quoted, or bare words. Bare words stop
# at a reserved word. Inside a function's parentheses they also stop at
# `by` and `order by`; in a segment's tail, at the tail words. A quoted
# name doubles a quote to hold one; a name holding `]` is quoted.
Field       <- Bracketed / Quoted / Word (SP Word)*
PField      <- Bracketed / Quoted / PWord (SP PWord)*
TField      <- Bracketed / Quoted / TWord (SP TWord)*
Word        <- !(Reserved WB) WordStart WordChar*
PWord       <- !((Reserved / BY / ORDER SP BY) WB) WordStart WordChar*
TWord       <- !((Reserved / FOR / LEFT / INNER / DEDUPE) WB) WordStart WordChar*
Bracketed   <- '[' (!']' .)+ ']'
Quoted      <- '"' ('""' / !'"' .)+ '"'

# ------------------------------------------------------------------- lexemes

# A word is defined by what it is not: whitespace, a quote at its start,
# or one of the characters the grammar uses structurally. So `P&L`,
# `Avg. Price`, `Share %` and names in any script are bare words, and
# a hyphen or an apostrophe inside a word is a letter.
String      <- "'" ("''" / !"'" .)* "'"
DString     <- '"' (!'"' .)* '"'                         # a SQL identifier inside calc(...)
Int         <- [0-9]+ WB
Space       <- [ \t\r\n]                              # the implementation also takes Unicode White_Space
Structural  <- [,()\[\]{}@=<>!"]
WordStart   <- !(Space / Structural / "'") .
WordChar    <- !(Space / Structural) .
WB          <- !WordChar                                 # word boundary
SP          <- [ \t]+
WS          <- [ \t]*
NL          <- '\r'? '\n'
_           <- [ \t\r\n]*
EOF         <- !.

Reserved    <- (AND / OR / NOT / IN / IS / BETWEEN / TOP / CONTAINS / STARTS / ENDS
              / KEYWORD / AS / ASC / DESC)
# Keywords fold case in ASCII only, so a Kelvin sign or an eszett never
# spells one.
AND <- 'and'i WB      OR <- 'or'i WB          NOT <- 'not'i WB        IN <- 'in'i WB
IS <- 'is'i WB        BETWEEN <- 'between'i WB TOP <- 'top'i WB        CONTAINS <- 'contains'i WB
STARTS <- 'starts'i WB  ENDS <- 'ends'i WB    KEYWORD <- 'keyword'i WB AS <- 'as'i WB
ASC <- 'asc'i WB      DESC <- 'desc'i WB
BY <- 'by'i WB        ORDER <- 'order'i WB    WITH <- 'with'i WB      NULL <- 'null'i WB
FOR <- 'for'i WB      LEFT <- 'left'i WB      INNER <- 'inner'i WB    DEDUPE <- 'dedupe'i WB
NEQ <- 'neq'i WB      LT <- 'lt'i WB          NONE <- 'none'i WB
LIMIT <- 'limit'i WB  VIA <- 'via'i WB        INCLUDE <- 'include'i WB EXCLUDE <- 'exclude'i WB
EXPAND <- 'expand'i WB  CALC <- 'calc'i WB    EXTRACT <- 'extract'i WB CUSTOM <- 'custom'i WB
```

Errors. Ordered choice means a malformed filter or alias would
otherwise succeed as a shorter projection and fail later with
"expected a comma". Two devices prevent that. First, `ItemEnd` guards
the end of every item: what follows must be a separator, and the
guards for what else could be there each carry a message, so `monthly(`
is "unknown function monthly", a reserved word is "`in` ends the name
Row; write [Row in region]", `@x` is "expected @d or @m", and a stray
quote is "a quote opens a name or value only at the start of a word".
Second, the parser commits after an opener (`limit`, a segment's `(`,
a function's `(`, a comparison operator, `between`, `top`, `as`, `by`,
`order by`, the comma inside `calc(`, `[`, `"`, `'`), so the error is
reported inside the construct that was begun, naming what it needs:
`not =` says "write !=", a formula's comma says which types may follow,
and an unclosed quote names the column it was opened at. Raw JSON is
handed to serde after the braces are balanced, and its line and column
are mapped back to the input.

Three properties of the grammar carry the design:

Ordered choice does the classifying. An item is tried as a setting,
a segment, raw JSON, a filter and last a projection, and the first
reading that parses wins. Because `Setting` needs a number after
`limit` and `Deco` needs a `(` after a function name, `limit` alone,
`include tax` and `monthly sales` fall through to projections of fields
with those names. Only the fourteen reserved words are reserved
everywhere.

Lookahead makes keywords contextual. `PWord` refuses `by` and `order
by` only inside a function's parentheses, `TWord` refuses the tail
words only after a segment, and `Text` stops at `and` and `or` only
where a filter tree could continue. Everywhere else those are letters,
so `order date`, `created by` and `null count` are bare names.

Nothing is resolved while parsing. Whether a name is a dimension, a
measure, a projection alias or nothing at all is the spec lowering's
business. The grammar produces a tree of what was written, and the
checks that need the model (kinds, projected partition references, the
predicate a type allows) are listed under the sections that follow.

## Decorators, as functions

```
month(order date)              extract(day_of_week, order date)   yoy(store net paid)
yoy_pct(store net paid)        share(store net paid)              share(store net paid by region)
moving_avg(store net paid, 7)  running_sum(store net paid by region)
rank(store net paid by region order by order date desc)
ntile(store net paid, 4)       lag(store net paid, 1)             custom('upper(@expression)', country)
```

| Family | Spelling | Spec |
|---|---|---|
| truncate | `hour() day() week() month() quarter() year()`, also `raw() minute() second() millisecond()` | `truncate.grain` |
| extract | `extract(part, x)` with part one of `minute hour day_of_month day_of_week day_of_year week_of_year month quarter year day_name month_name year_month` | `extract.extract` |
| temporalize | `yoy() qoq() mom() wow() dod()` for the prior period's value, `yoy_pct()` and the rest for the relative change | `temporalize.transform`, `percent_change` |
| contribute | `share(x)`, `share(x by a, b)`; `share_filtered(x by a)` keeps the query's filters in the total | `contribute.partition_refs`, `ignore_partition_filters: false` |
| window, frame | `running_sum running_avg running_min running_max running_count`; `moving_sum(x, 7)` and the other four moving forms | `window.mode`, `function`, `size` |
| window, rank | `rank dense_rank row_number percent_rank cume_dist first_value last_value`, `ntile(x, 4)`, `lag(x, 1)`, `lead(x, 1)` | `window` with `mode: running`, `buckets`, `offset` |
| customize | `custom('sql with @expression', x)` | `customize.custom_sql` |

The number after a moving function is the number of rows in the frame,
the current row included, which is what "a seven-day moving average"
means. The spec's `size` counts the rows before the current one, so
`moving_avg(x, 7)` lowers to `size: 6`, and `moving_avg(x, 1)` is an
error because a frame has at least two rows. For `ntile`, `lag` and
`lead` the number is the spec's `buckets` or `offset` as written.

A reference after `by` or `order by` is a projected dimension, by its
name, uid or alias, never an expression: `order by order date`, not
`order by month(order date)`, because the window orders over the
projection and the projection already carries the truncation. A decorator's one
boolean flag is spelled as a suffix on the function name, `yoy_pct`
and `share_filtered` alike; the spelling `share(x by a with filters)`
was dropped because it gave `with` a third meaning.

Functions nest: `yoy(month(order date))` is wrong on kind and the
planner says so, `share(yoy(store net paid))` is a share of the yoy
value.

## Calculations

```
calc(sum([Store Net Paid]@m) / sum([Catalog Net Paid]@m)) as Share
calc(count(*), integer) as Orders
yoy_pct(calc(sum([Store Net Paid]@m) / sum([Catalog Net Paid]@m))) as Growth desc
calc(1000) as Target
```

A calculation is `calc(...)` around its SQL, and nothing else is: `=`
only ever means a filter, so the parser never reads SQL to classify
an item. The alias follows as `as`, and when none is given the
formula's text is its own alias, as now. The result type is an
optional last argument after a top-level comma. The lexer finds the
closing parenthesis by balancing parentheses and single quotes, so
`coalesce([A]@m, 0)` and `'a, b'` keep their commas; a top-level comma
not followed by a type is an error naming the types. Because
`calc(...)` is an operand like a field, a decorator wraps it the way
it wraps a field, and nothing is peeled.

Inside a formula, references keep the spec's own bracket forms,
`[Name]@m`, `[Name]@d` and `[Alias]`, because the spec and the planner
already read them, and because double quotes inside SQL are column
identifiers, which calculations forbid. A uid in a formula is
bracketed like a name, `[store-net-paid]@m`, so its hyphens are never
read as subtraction.

## Filters

```
country = Canada                     name contains made in china
revenue > 100                        name not starts with x
order date between 2024-01-01 and 2024-12-31
item category in (Books, Music)      item category not in (Toys, Games)
order date is not null               category top 10 by revenue
order date >= 28d                    description keyword (sale, clearance)
net paid@m > 1000                    (country = Canada or region = West) and revenue > 10
"Is Active" = true                   [Sign In Date] >= 7d
name = 'Rock and Roll'               title = 'Toys, games (new)'
```

Commas between items are AND, as now. `and`, `or` and parentheses
build the spec's tree; a query whose filters hold no `or` lowers to
the flat array, so the common case keeps its old shape. A value is
plain text up to a separator, so it may contain `in`, `top` or `desc`
unquoted; `between` claims its own `and` before the tree sees one. A
value holding a comma, a parenthesis, `and` or `or` is single-quoted.
Inside a list a value runs to the next comma or the closing
parenthesis, so `in (Rock and Roll, Jazz)` is two values without
quotes. The grammar reads every value as text; the lowering writes a
value that is wholly an integer or a decimal as a JSON number, `true`
and `false` as booleans, and everything else as the text written, so
`02134` or `1.10` are single-quoted when they must stay strings. A
measure filter is a HAVING, as in the spec.

`not` goes before the predicate word and only before the four that
have a negated form in the spec (`in`, `contains`, `starts with`, `ends
with`), with `is not null` accepted as the English. `not =` is an
error pointing at `!=`; `not between` and `not (...)` are errors,
since the spec has neither. `in` and `keyword` always take a
parenthesised list, even of one value.

The spec joins a list into one comma-separated string and the planner
splits it again, so a list value can never hold a comma; the parser
refuses `in ('Toys, games')` with that reason rather than lowering
something the planner will misread. A single value may hold a comma
when quoted: `title = 'Toys, games'`.

## Segments

```
include (customer id, catalog net paid > 0)
exclude (customer id, store net paid > 1000) for store net paid
include (customer id, item category = Books, order date >= 1y) as Readers
include (customer id, store net paid@m)
expand (customer id, item category = Books) with (item category as Also bought) left dedupe none as Book pairs
```

Inside the parentheses the items are read by the same rules as the
query: anything with a predicate is a filter, trees included, and a
lone name is a key. The lowering has no model, so it cannot tell a
measure from a dimension by its name; a measure that defines
membership through its fact table is marked `@m`, and the spec rejects
an unmarked measure under keys with a message that says where to list
it. `include` and `exclude` are the spec's modes. `expand` is the
expanding segment, whose carried dimensions sit in `with (...)`
because they are not keys; each may take `as` for its column name,
and the spec's default `<Field> (same <Key>)` applies when it does
not. The segment's own name is `as` in the tail, outside the
parentheses. Tails come in any order, and a bare segment name stops at
a tail word, so a name holding `for`, `left`, `inner` or `dedupe` is
quoted.

## Settings and the escape hatch

```
limit 100
via (store-sales, store)
{"name": "Monthly growth", "db_settings": {"week_start_day": 1}}
store net paid {"format": {"type": "currency"}}
```

`limit` and `via` (table hints) are the two settings that change the
plan and earn a keyword. Everything else the spec carries, and any
attribute a future spec adds, goes through braces: a `{...}` item is
merged into the query object, and a `{...}` directly after an item,
with no comma between, is merged into that item's object. Merging is
deep for objects, appends for arrays and replaces scalars. The hatch
is what makes parity complete without a keyword per attribute. The
spec's `axis` and `hidden` are not handled at all: both are leaving the
spec, and the planner's one use of `axis`, choosing an automatic
partition, is replaced by an explicit `by`.

## Parity against the spec

| Spec level | Attributes | Native | Through the hatch |
|---|---|---|---|
| query | name, description, classification, limit, projections, calculations, filters, segments, view, db_settings, hints | 6 | name, description, classification, view, db_settings |
| projection | field, field_type, alias, order_by, format, decorators, calculation, sql, data_type | 8 | format |
| decorator | type, grain, extract, transform, percent_change, mode, function, size, offset, buckets, partition_refs, order_by, ignore_partition_filters, custom_sql | 14 | |
| filter | field, field_type, predicate, value, value_end, top_n_measure, and/or tree | 7 | the `custom` predicate |
| segment | name, mode, keys, measures, filters, apply_to, expanding field, expanding alias, join, pair_dedupe | 10 | |

45 of 51 attributes have a native spelling and all 51 are reachable;
`axis` and `hidden` are left out of the count because they are leaving
the spec. The six without are carried, not planned, and the `custom`
predicate, which the planner does not implement, is not given a
keyword for something that cannot run. `field_type` is the `@d` or
`@m` suffix, lowered to the attribute rather than left on the name.
Nineteen of the twenty predicates are native.

Two gaps are the spec's rather than the grammar's, and the language
should not paper over them. First, a window's `order_by` keys and a
window's or contribute's `partition_refs` are kept only when they are
uids of projected dimensions; a name or alias is dropped silently
today. The expression lowering has no model and emits what was
written, so the spec lowering must resolve these references against
the projected dimensions, by uid, name or alias, and fail on a miss,
at the point where it now prunes them. That is a small change in
`crates/spec`, and until it lands a partition written as a name is
lost. Second, a window cannot order by a measure, only by a projected
dimension, so `rank(x order by x desc)` has no spec to lower to and is
reported as such.

## Hazards and how the grammar handles them

`order date` against `order by`. Both are common, and the first is a
bare name. `order` is a keyword only when the next word is `by`, so
`running_sum(x by order date)` partitions by the date and `order by
order date desc` orders by it. A field literally named "Order By
Status" is bracketed.

`between a and b` against the `and` of a tree. `between` consumes the
first `and` after its lower bound; the upper bound then runs to the
next `and`, `or`, comma or newline, so `date between 1y and 0d and
country = Canada` reads as two predicates. A value that itself holds
`and` or `or`, such as `Rock and Roll`, is single-quoted; unquoted,
the parser fails at `Roll` and says a value holding `and` needs
quotes.

A reserved word inside a name. `Is Active`, `Top Seller` and `Sign In
Date` all end early. The parser reports the reserved word, where it
stopped, and the bracketed spelling to use. The list is fourteen
words, down from nineteen, by making `by`, `null`, `with` and `custom`
contextual or unnecessary and by dropping `hidden` and `axis`.

`by` inside a function. `created by` is a bare name at the top level
because `by` is only a keyword after `top N` and inside a function's
parentheses. Inside `share(x by created by)` the first `by` wins and
the error says to bracket the name.

A list value holding a comma. The spec cannot carry it, so the parser
refuses it rather than lowering a list the planner will split. A
single value may, when quoted.

Commas inside `calc(...)`. The closing parenthesis is found by
balancing parentheses and single quotes, so nested calls and string
literals keep their commas. A top-level comma is only ever the type
argument.

Newlines. A newline separates items at the top level and inside a
segment's parentheses, where items are the same kind of thing. Inside
a list, a function's arguments or a formula it is whitespace, so a
long `in (...)` or a long formula may be wrapped.

`as` on a carried dimension against `as` on a segment. The carried
alias sits inside `with (...)`, the segment's name sits in the tail,
and the parentheses keep them apart. The same `as` on a projection is
the alias, and because `asc`, `desc` and `{` are reserved or
punctuation, an alias stops at them in any order.

`with`. It follows `starts`, `ends` or an `expand (...)` group and is a
plain word anywhere else. The contribute flag no longer uses it.

`not`. One position, before the predicate word, and only before the
four predicates that negate. The English `is not null` is the one
exception, because `is not` is what SQL says.

`@m` after a quoted name. The suffix attaches with no space to any
field form, `"Net Paid"@m` and `[Net Paid]@m` alike, and an `@` never
starts a value, so no filter value is mistaken for a suffix. A `@` inside
a value, as in an email address, is text.

Hyphens. A `-` is a letter, so dates, relative dates and uids are one
word each, and `revenue > -5` reads `-5` as the value's text, which
the lowering writes as a number. Subtraction only exists inside
`calc(...)`, where the SQL is opaque and a uid is bracketed.

A function name as a field. `month` alone is a field; `month(` is a
function. A field whose name ends in a parenthesised phrase, such as
"Count (Orders)", is bracketed.

An item-leading word in a name. `include tax` at the start of an item
is tried as a segment, which needs a `(`, and falls through to a
projection of that field. The parser commits to a segment only once
the parenthesis is seen, so an error inside a real segment is
reported inside it.

Keys against measures in a segment. The lowering cannot resolve names,
so a lone name is a key unless it carries `@m`. An unmarked measure
reaches the spec's own error, which names the fix.

## Examples

A small shop model: the dimensions `order date`, `country`, `region`,
`item category` and `customer id`, and the measures `store net paid`,
`catalog net paid` and `orders`. Each line below exercises one
construct, and the first several carry the spec they lower to, so
they double as golden candidates. Names are emitted as written; the
planner resolves them to uids.

```
item category, store net paid desc
```
```json
{"projections": [{"field": "item category"}, {"field": "store net paid", "order_by": "desc"}]}
```

```
month(order date), store net paid, yoy_pct(store net paid) as Growth
```
```json
{"projections": [
  {"field": "order date", "decorators": [{"type": "truncate", "grain": "month"}]},
  {"field": "store net paid"},
  {"field": "store net paid", "alias": "Growth",
   "decorators": [{"type": "temporalize", "transform": "year_over_year", "percent_change": true}]}]}
```

```
region, store net paid, order date between 2024-01-01 and 2024-12-31, item category in (Books, Music)
```
```json
{"projections": [{"field": "region"}, {"field": "store net paid"}],
 "filters": [
  {"field": "order date", "predicate": "between", "value": "2024-01-01", "value_end": "2024-12-31"},
  {"field": "item category", "predicate": "in_list", "value": "Books, Music"}]}
```

```
item category, orders, item category top 5 by store net paid
```
```json
{"projections": [{"field": "item category"}, {"field": "orders"}],
 "filters": [{"field": "item category", "predicate": "top_n", "value": 5, "top_n_measure": "store net paid"}]}
```

```
region, item category, store net paid, share(store net paid by region) as Share of region
```
```json
{"projections": [
  {"field": "region"}, {"field": "item category"}, {"field": "store net paid"},
  {"field": "store net paid", "alias": "Share of region",
   "decorators": [{"type": "contribute", "partition_refs": ["region"]}]}]}
```

```
day(order date), region, store net paid, moving_avg(store net paid, 7 by region order by order date) as 7 day avg
```
```json
{"projections": [
  {"field": "order date", "decorators": [{"type": "truncate", "grain": "day"}]},
  {"field": "region"}, {"field": "store net paid"},
  {"field": "store net paid", "alias": "7 day avg",
   "decorators": [{"type": "window", "mode": "moving", "function": "avg", "size": 6,
                   "partition_refs": ["region"], "order_by": {"order date": "asc"}}]}]}
```

```
country, store net paid, (country = Canada or region = West) and store net paid > 1000
```
```json
{"projections": [{"field": "country"}, {"field": "store net paid"}],
 "filters": {"and": [
  {"or": [{"field": "country", "predicate": "equals", "value": "Canada"},
          {"field": "region", "predicate": "equals", "value": "West"}]},
  {"field": "store net paid", "predicate": "greater_than", "value": 1000}]}}
```

```
country, calc(sum([Store Net Paid]@m) / sum([Catalog Net Paid]@m)) as Store to catalog, calc(count(*), integer) as Rows
```
```json
{"projections": [{"field": "country"}],
 "calculations": [
  {"calculation": true, "alias": "Store to catalog", "sql": "sum([Store Net Paid]@m) / sum([Catalog Net Paid]@m)"},
  {"calculation": true, "alias": "Rows", "sql": "count(*)", "data_type": "integer"}]}
```

```
customer id, store net paid, include (customer id, catalog net paid > 0) as Catalog buyers
```
```json
{"projections": [{"field": "customer id"}, {"field": "store net paid"}],
 "segments": [{"name": "Catalog buyers", "mode": "include", "keys": ["customer id"],
               "filters": [{"field": "catalog net paid", "predicate": "greater_than", "value": 0}]}]}
```

```
item category, orders, expand (customer id, item category = Books) with (item category as Also bought) left dedupe none as Book pairs
```
```json
{"projections": [{"field": "item category"}, {"field": "orders"}],
 "segments": [{"name": "Book pairs", "keys": ["customer id"],
               "filters": [{"field": "item category", "predicate": "equals", "value": "Books"}],
               "expanding": [{"field": "item category", "alias": "Also bought"}],
               "join": "left", "pair_dedupe": "none"}]}
```

The remaining lines cover the rest of the constructs; their specs
follow from the ones above.

```
region, store net paid, catalog net paid, exclude (customer id, store net paid > 1000) for store net paid
include (customer id, store net paid@m), customer id, catalog net paid
month(order date), running_sum(orders) as Orders to date
region, item category, store net paid, row_number(store net paid by region) as "Row in region"
extract(day_of_week, order date) as Weekday, orders
custom('upper(@expression)', country) as Country, orders
item category, orders, item category keyword (book, music)
month(order date), yoy_pct(calc(sum([Store Net Paid]@m) / sum([Catalog Net Paid]@m))) as Mix growth desc
item category, store net paid, item category not in (Toys, Games), item category not starts with X
"Is Active" = true, customer id, store net paid@m > 100
store net paid, order date >= 28d, limit 10, via store-sales
country, store net paid {"format": {"type": "currency"}}, {"name": "Paid by country"}
```

The last two show a setting, a hint, and both uses of the hatch. A
multi-line form is the same query with newlines for commas:

```
month(order date), region
store net paid, share_filtered(store net paid by region) as Share
order date >= 1y, region in (West, East)
include (customer id, catalog net paid > 0) as Catalog buyers
limit 500
```

## What this costs, and the decisions in it

Fourteen reserved words cannot appear inside an unbracketed name. The
ones likely to collide are `is` (Is Active), `top` (Top Seller), `in`
(Sign In Date), `as` and `not`. The parser reports a reserved word in
name position with the quoted spelling to use. Making `by`, `order`,
`with`, `for` and `left` contextual spares Created By, Order Date,
Reason For Return and Left Hand.

`calc(...)` for calculations. One extra word buys a grammar in which
`=` is only a filter, a constant or a `case` expression needs no
special spelling, and a decorator wraps a formula exactly as it wraps
a field.

Prefix functions, one spelling each. `month(x)` truncates,
`extract(month, x)` pulls the part, `yoy_pct(x)` is the relative
change, `share_filtered(x by a)` keeps the filters in the total. No
postfix form, no `pct(x)` alias for share, no `dow` beside
`day_of_week`, no `does not contain` beside `not contains`.

Lists after a keyword are one name or several in parentheses (`for
revenue`, `for (revenue, orders)`, `via (a, b)`), so a trailing list
never runs into the next item. Lists of values, after `in` and
`keyword`, are always parenthesised.

A moving window's number counts rows in the frame, not the spec's
rows-before-current, because the person writing it is answering "a
seven-day moving average" and the spec's convention is an
implementation detail the lowering can absorb.

`format` goes through the hatch, a display hint written rarely and
never by a model asking for SQL. `axis` and `hidden` are not handled:
they are leaving the spec.

The `custom` predicate has no spelling because the planner refuses it;
giving it a keyword would reserve a word for an error.

Bracket references stay inside formulas, since SQL text cannot carry
bare multi-word names and the planner substitutes by name.

## Implementation sketch

`zsql-shorthand` is rewritten in place, keeping its name and its two
entry points: a hand-written recursive-descent parser over the input
bytes, with no separate lexer and no parser
library, producing a typed AST whose every node carries a byte span,
then a lowering to the spec `Value`, and `describe` from the AST rather
than the JSON. No lexer because the grammar is scannerless and
mode-dependent: a value, a list value, a formula and raw JSON are spans
whose end depends on where the parser stands, and `by`, `order`,
`with`, `for`, the function names, parts and types are keywords only
in position. A token stream cannot carry that without lexer modes,
and a cursor parser reads those spans on demand with a handful of
helpers (a word, a keyword with ASCII folding and a boundary, an
integer, a string, a raw span to a stop set). No library because the
error messages above are each a commit point plus a sentence, which
hand-written code states directly and pest or nom cannot. Spans are
byte offsets, turned into line and column only when an error is
rendered. The PEG in this document is the specification and the
goldens are the proof that the code agrees with it. The crate also
answers where a cursor stands: `complete::context` reads the frames a
half-typed line is inside and says what the grammar can take there, a
field of a kind, a function, a keyword, a projected dimension after
`by`, a value after a predicate, a table after `via`, a part or a type.
The repl's line helper maps those offers to the model for tab
completion, the dimmed hint as you type, bold keywords, and a red
unclosed opener that keeps the line open for another line. The spec lowering
in `crates/spec` now resolves partition and order references by uid,
name or alias against the projected dimensions and refuses a miss,
which `crates/golden/tests/spec.rs` pins. The lowering is
model-free: it emits names as written, lowers `@d`/`@m` to
`field_type`, turns a moving frame's row count into `size`, and flattens
a tree with no `or` into the array form. Resolution of partition and
order references moves into the spec lowering, which has the universe
and the projections, and fails on a miss instead of pruning. `HELP`
becomes a generated summary of the grammar. Goldens: a text file of
lines beside their expected specs, in the pattern of the existing
golden crate, with one line per construct in this document plus the
failure cases (reserved word in a name, unclosed quote, unknown
function, a list value with a comma, `not =`, `moving_avg(x, 1)`, an
unbalanced calc, a double-quoted value, a repeated modifier, `limit`
with a decimal). Every example line and every hazard input in this
document is a golden. The request field stays `expr`; MCP still refuses
it, and whether the denser form changes that is a question for the
eval harness.
