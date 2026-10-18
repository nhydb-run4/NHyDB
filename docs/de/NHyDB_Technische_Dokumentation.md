# NHyDB 0.27.0 – Technische Dokumentation

| Metadatum | Wert |
|---|---|
| Dokument | Technische Referenzdokumentation NHyDB 0.27.0 (native Rust-Engine mit Python-Anbindung) |
| Sprache | Deutsch |
| Dokumentstand | 2026-10-17 |
| Produkt | NHyDB 0.27.0, Python-Paket `nhydb` |
| Engine-Version (`ENGINE_VERSION`) | `0.27.0` |
| Dateikennung | `NHYDB016` (Formatrevision 16, `FILE_FORMAT_VERSION = "0.16"`) – interne Kennung des Dateiformats; sie ändert sich nur, wenn sich das Format auf dem Datenträger ändert, und ist unabhängig von der Produktversion |
| Plattformen | macOS, Linux, Windows |
| Lizenz | NHyDB / dNHyDB Experimental Software License 1.0, siehe Datei `LICENSE.md` (quelloffen einsehbar, keine Open-Source-Lizenz; keine produktive Nutzung) |
| Kontakt | achim.meissner@nhydb.de |

---

## Inhaltsverzeichnis

1. [Zielsetzung und Einordnung](#1-zielsetzung-und-einordnung)
2. [Mathematische Grundlagen der Adressierung](#2-mathematische-grundlagen-der-adressierung)
3. [Umsetzung der Mathematik im Rust-Kern](#3-umsetzung-der-mathematik-im-rust-kern)
4. [Architektur und Komponenten](#4-architektur-und-komponenten)
5. [Datenmodell: Realms, Shapes, Extensions, Layouts](#5-datenmodell-realms-shapes-extensions-layouts)
6. [Datentypen und Wertkodierung](#6-datentypen-und-wertkodierung)
7. [Speicherorganisation und Dateiformat 0.16](#7-speicherorganisation-und-dateiformat-016)
8. [Systemadressraum R0,u1 und externer Layoutplan](#8-systemadressraum-r0u1-und-externer-layoutplan)
9. [Schreib- und Lesepfade, Commit und Persistenz](#9-schreib--und-lesepfade-commit-und-persistenz)
10. [Layouts, Routing und Layout-Evolution](#10-layouts-routing-und-layout-evolution)
11. [Geometrisches GET: Zelle, Record, Auto](#11-geometrisches-get-zelle-record-auto)
12. [Sekundärindizes und Volltextsuche](#12-sekundärindizes-und-volltextsuche)
13. [MATH_MAP: mathematische Belegung](#13-math_map-mathematische-belegung)
14. [Abfragesysteme: QueryEngine und NHySQL-0.24](#14-abfragesysteme-queryengine-und-nhysql-024)
15. [Integrität, Recovery und Sicherheitsmechanismen](#15-integrität-recovery-und-sicherheitsmechanismen)
16. [Python-Schnittstelle (PyO3) und NumPy](#16-python-schnittstelle-pyo3-und-numpy)
17. [Konfiguration, Installation, Build und Betrieb](#17-konfiguration-installation-build-und-betrieb)
18. [Tests, Validierung und Benchmarks](#18-tests-validierung-und-benchmarks)
19. [Bekannte Einschränkungen](#19-bekannte-einschränkungen)
20. [Anhang: Referenzen im Quellcode](#20-anhang-referenzen-im-quellcode)

---

## 1. Zielsetzung und Einordnung

### 1.1 Was NHyDB ist

NHyDB ist eine eingebettete, dateibasierte Datenbank-Engine, deren Adressraum
nicht durch Tabellenzeilen oder Schlüssel, sondern durch eine **arithmetische
Zellgeometrie** definiert ist. Jede speicherbare Zelle besitzt die strukturelle
Identität

```text
(R, u, v, lane)      R = Realm, u = erste Koordinate, v = lokale zweite Koordinate,
                     lane ∈ {NL, NR} (Spur/Seite)
```

Aus dieser Viererkoordinate lässt sich jederzeit eine eindeutige natürliche
Zahl `N` berechnen – und umgekehrt. `N` ist eine **abgeleitete externe
Referenz**, kein gespeicherter Schlüssel.

Die vorliegende Implementierung ist eine **native Rust-Engine** mit
Python-Anbindung über PyO3.

### 1.2 Funktionsumfang (Überblick)

| Bereich | Inhalt |
|---|---|
| Adressmathematik | exakte Abbildung `(R,u,v,lane) ↔ N` mit beliebig großen Ganzzahlen; Ausschluss von GZ- und N0-Zahlen |
| Speicherkern | Dense-BASE-Records, Sparse-Zellen, zwei Lanes, Tombstones und Null-Werte als getrennte Zustände |
| Logische Interpretation | Layouts CLASSIC, COLUMN, PACKED, CUSTOM; Mixed-Realm-Routing; sichere Layout-Evolution |
| Lesen | `get`, `get_cell`, Objekt-APIs, geometrisches GET (`cell`/`record`/`auto`), DESCRIBE, Belegungsabfragen |
| Persistenz | Append-only-Container, alternierende Superblöcke, Commit-Grenze, Tail-Recovery, Durability `fsync`/`full` |
| Sicherheit | Systemadressraum R0,u1, verschlüsselter externer Layoutplan (AES-256-GCM, HMAC-SHA256), fail-closed Öffnen, Schreibsperre, DB-Größengrenze |
| Indizes | VALUE, TOKEN, TRIGRAM, FULLTEXT (Positionen, BM25) |
| MATH_MAP | sichere Ausdruckssprache, Abbildung von Eingabetupeln auf Zellen, Realmplaner, exklusive MATH-Realms |
| Abfragen | QueryEngine (Kostenplaner, Provider, Aggregation, Ranking) und NHySQL-0.24 (lesendes `SELECT`) |
| Wartung | Checkpoint, Verify, Health, Stats, Repair, Kompaktierung (in-place und Export) |
| Python | Python-API (Einzeloperationen) und NumPy-Batch-API |

### 1.3 Abgrenzung

NHyDB ist **kein** relationales Datenbanksystem, **kein** Server und
**kein** Mehrprozess-Transaktionssystem. Es gibt genau einen schreibenden
Prozess je Datenbank (abgesichert durch eine Sperrdatei), keine
Netzwerkschnittstelle und keine explizite Transaktions-API mit
`BEGIN`/`ROLLBACK`. NHySQL ist eine lesende Abfragesprache; Schreibzugriffe
erfolgen ausschließlich über die API.

---

## 2. Mathematische Grundlagen der Adressierung

Dieser Abschnitt beschränkt sich auf die Mathematik, die im
Quellcode **tatsächlich implementiert** ist. Jede Formel ist einer
Rust-Funktion zugeordnet; die Rechenbeispiele wurden mit der Engine
nachgerechnet (Python-Aufrufe in das native Modul) bzw. sind Testfälle aus
`crates/nhydb-core/src/address.rs` und `tests/python/test_math.py`. Wie die
Formeln im Kern umgesetzt sind (Datentypen, Rechenpfade), beschreibt
Abschnitt 3.

### 2.1 Größen und Wertebereiche

| Symbol | Bedeutung | Wertebereich (im Code geprüft) | Rust-Bezeichner |
|---|---|---|---|
| `R` | Realm-Nummer | ganze Zahl `R ≥ 1`, sonst `ValueError("realm_id must be > 0")` | `realm`, `realm_id` |
| `u` | erste Koordinate | `u ≥ 1`, sonst `ValueError("u must be > 0")` | `u` |
| `v` | **lokale** zweite Koordinate | `1 ≤ v ≤ T`; `v = 0` ⇒ `RealmLimitError("local v must start at 1")`, `v > T` ⇒ `RealmLimitError("Realm R: local v limit reached …")` | `v_local`, `local_v` |
| `lane` | Spur | genau `0` (NL) oder `1` (NR) | `Lane = u8`, `NL = 0`, `NR = 1` |
| `T` | Realm-Spannweite | Konstante `10^100` | `realm_span()` |
| `V` | globale zweite Koordinate | `V ≥ 1` | `vg`, Feld `v` der Adress-Dicts |
| `G` | Gruppe | `G = u + V ≥ 2` | `g` |
| `N` | kanonische Zellzahl | `N ≥ 3`; GZ- und N0-Zahlen ausgeschlossen | `n` |

Alle Größen sind **unbeschränkte** natürliche Zahlen. Es gibt keine
Fließkommarechnung und keine Begrenzung auf 64 Bit für mathematische
Koordinaten (zur physischen Dekodiergrenze siehe Abschnitt 3.6).

> **Namensfalle `v`:** Funktionen erhalten das *lokale* `v`. In den
> Ergebnis-Dicts von `dynadron_address`, `cell_address` und
> `n_to_dynadron_address` steht unter `"v"` das *globale* `V`, das lokale unter
> `"v_local"`.

### 2.2 Spurnormalisierung (`normalize_lane_str`, `normalize_lane_int`)

| Eingabe | Ergebnis |
|---|---|
| `0`, `"0"`, `"NL"`, `"L"`, `"DEFAULT"` (Groß-/Kleinschreibung egal, Leerraum entfernt), `None` | `0` (NL) |
| `1`, `"1"`, `"NR"`, `"R"` | `1` (NR) |
| sonst | `ValueError("lane must be 0/NL or 1/NR")` bzw. `"lane must be 0 or 1"` |

Im Blockheader wird die Spur als Bit 0 der Flags gespeichert:
`flags = 1` für NR, `0` für NL (`lane_flags`, `lane_from_flags = flags & 1`).

### 2.3 Realm und globale Koordinate

Vorwärts (`global_v`):

```text
V = (R − 1)·T + v            (für R = 1 direkt V = v)
```

Rückwärts (`split_global_v`):

```text
R = ⌊(V − 1) / T⌋ + 1
v = ((V − 1) mod T) + 1
```

Realmgrenzen (`realm_v_bounds`, API `realm_bounds`):

```text
V_min(R) = (R − 1)·T + 1,   V_max(R) = R·T
```

**Rechenbeispiele (mit der Engine geprüft):**

| Aufruf | Ergebnis |
|---|---|
| `split_global_v(T)` | `(R = 1, v = T)` |
| `split_global_v(T + 1)` | `(R = 2, v = 1)` |
| `global_v(2, 1)` | `T + 1` |
| `realm_v_bounds(2)` | `(T + 1, 2T)` |

`V = T` ist die letzte Zelle von Realm 1, `V = T + 1` die erste von Realm 2.
Für jede Kombination `(R, u, lane)` beginnt das lokale `v` wieder bei 1.

### 2.4 Gruppe und Vorwärtsabbildung auf N (`n_pair`, `dynadron_n`)

```text
G = u + V

lane = NL (0):   N = G² − u
lane = NR (1):   N = G² + V
```

Eigenschaften, die direkt aus den Formeln folgen und in den Tests geprüft
werden:

* Wegen `1 ≤ u ≤ G − 1` liegt `N_NL` in `[G² − G + 1, G² − 1]`, wegen
  `1 ≤ V ≤ G − 1` liegt `N_NR` in `[G² + 1, G² + G − 1]`.
* Dieselbe Strukturkombination `(R, u, v)` ergibt zwei verschiedene Zellen
  (NL und NR); beide sind unabhängig voneinander speicherbar.

**Rechenbeispiele:**

| `(R, u, v, lane)` | `V` | `G` | Rechnung | `N` |
|---|---|---|---|---:|
| `(1, 1, 1, NL)` | 1 | 2 | `4 − 1` | 3 |
| `(1, 1, 1, NR)` | 1 | 2 | `4 + 1` | 5 |
| `(1, 2, 1, NL)` | 1 | 3 | `9 − 2` | 7 |
| `(1, 1, 2, NL)` | 2 | 3 | `9 − 1` | 8 |
| `(1, 2, 1, NR)` | 1 | 3 | `9 + 1` | 10 |
| `(1, 1, 2, NR)` | 2 | 3 | `9 + 2` | 11 |
| `(1, 5, 2, NL)` | 2 | 7 | `49 − 5` | 44 |
| `(1, 3, 5, NL)` / `NR` | 5 | 8 | `64 − 3` / `64 + 5` | 61 / 69 |
| `(1, 11, 13, NL)` / `NR` | 13 | 24 | `576 − 11` / `576 + 13` | 565 / 589 |
| `(23, 14, 744, NL)` | `22·T + 744` | `22·T + 758` | `G² − 14` | 203 Dezimalstellen |
| `(2, 3, 5, NR)` | `T + 5` | `T + 8` | `G² + T + 5` | 201 Dezimalstellen |

### 2.5 Ausgeschlossene Zahlen GZ und N0

Für jede Gruppe `G ≥ 2`:

```text
N0(G) = G·(G − 1)        Gruppengrenze, keine Zelle
GZ(G) = G²               Quadratzentrum, keine Zelle
NL-Zellen:  N0(G)+1 … GZ(G)−1        (G − 1 Zahlen)
NR-Zellen:  GZ(G)+1 … N0(G+1)−1      (G − 1 Zahlen),  N0(G+1) = G·(G+1)
```

Damit zerfällt jedes Intervall `[G(G−1), (G+1)G)` in genau
`N0(G)`, `G−1` NL-Zellen, `GZ(G)`, `G−1` NR-Zellen. Jede positive ganze Zahl
ist entweder genau eine Zelle oder genau eine der beiden Grenzarten (für
`N = 1, 2` gilt: `1 = GZ(1)`, `2 = N0(2)`; der Kerntest prüft alle Zahlen von
1 bis 199 999 auf diese Zerlegung).

Beispiel `G = 24`: `552 = N0(24)`, NL-Zellen `553 … 575`, `576 = GZ(24)`,
NR-Zellen `577 … 599`, `600 = N0(25)`.

### 2.6 Rückwärtsabbildung N → (R, u, v, lane) (`n_to_address`)

Algorithmus (nur ganzzahlige Operationen):

```text
Eingabe N ≥ 1 (N = 0 ⇒ ValueError "N must be > 0")
m = isqrt(N)                 größtes m mit m² ≤ N
d = N − m²                   es gilt 0 ≤ d ≤ 2m

d = 0          ⇒ GZ(m):   AddressError "N… is a GZ point, not an NHyDB storage cell"
1 ≤ d < m      ⇒ NR:      G = m,     V = d,          u = m − d
d = m          ⇒ N0(m+1): AddressError "N… is an N0 point, not an NHyDB storage cell"
m < d ≤ 2m     ⇒ NL:      G = m + 1, u = G² − N,     V = G − u

(R, v) = split_global_v(V)
Kontrolle: cell_address(R, u, v, lane).N == N, sonst AddressError
           "N… could not be resolved unambiguously"
```

Begründung der Fallunterscheidung: Für NR-Zellen gilt `G² < N < G² + G`, also
`isqrt(N) = G` und `d = V ∈ [1, G−1]`. Für NL-Zellen gilt
`(G−1)² + (G−1) < N < G²`, also `isqrt(N) = G − 1 = m` und
`d = N − m² ∈ [m+1, 2m]`. `d = m` entspricht `N = m(m+1) = N0(m+1)`.

**Rechenbeispiele:**

| `N` | `m` | `d` | Fall | Ergebnis |
|---:|---:|---:|---|---|
| 589 | 24 | 13 | `1 ≤ 13 < 24` ⇒ NR | `G = 24, V = 13, u = 11` ⇒ `(1, 11, 13, NR)` |
| 391 | 19 | 30 | `30 > 19` ⇒ NL | `G = 20, u = 400 − 391 = 9, V = 11` ⇒ `(1, 9, 11, NL)` |
| 553 | 23 | 24 | `24 > 23` ⇒ NL | `G = 24, u = 23, V = 1` (erste NL-Zelle von G = 24) |
| 575 | 23 | 46 | `46 = 2m` ⇒ NL | `G = 24, u = 1, V = 23` (letzte NL-Zelle) |
| 576 | 24 | 0 | GZ | `AddressError` |
| 552 | 23 | 23 | `d = m` ⇒ N0 | `AddressError` |
| 600 | 24 | 24 | `d = m` ⇒ N0 | `AddressError` |
| 10 | 3 | 1 | NR | `G = 3, V = 1, u = 2` ⇒ `(1, 2, 1, NR)` |

Für `N = (22·T + 758)² − 14` liefert die Umkehrung `V = 22·T + 744` und über
`split_global_v` exakt `(R = 23, v = 744)`.

### 2.7 Zusätzliche Kennzahlen (`dynadron_address`)

`dynadron_address(R, u, v)` liefert neben `R, u, v_local, V, G, NL, NR`:

```text
station        = V − 1
stufenbreite   = u + 2
stufe          = ⌊station / stufenbreite⌋ + 1
stufenstation  = station mod stufenbreite
```

Beispiel `(1, 3, 5)`: `V = 5, G = 8, station = 4, stufenbreite = 5,
stufe = 1, stufenstation = 4, NL = 61, NR = 69`. Für `(2, 3, 5)` gilt
`station = T + 4`, `stufe = ⌊(T+4)/5⌋ + 1`, `stufenstation = (T+4) mod 5`.
Diese Werte gehören **nicht** zur Zellidentität und werden nie gespeichert;
sie werden bei Bedarf berechnet und erscheinen in Zellausgaben mit dem Präfix
`_dyn_…`: `get_cell` und `get_cell_n` liefern sie standardmäßig
(`dyn_address=True`, abschaltbar mit `dyn_address=False`), `get_math_cell`
nur mit `dyn_address=True`. Für Record-Lesezugriffe (`get`, `range`) gilt der
Modus `dyn_address="off"|"once"|"auto"` (mit `dyn_v` für `"once"`); sein
Standardwert wird mit `set_dyn_address_output(mode, v=…)` gesetzt
(Voreinstellung `"off"`).

### 2.8 N-Referenzen als Text (`parse_n_reference`)

Akzeptiert werden Dezimalzahlen mit optionalem führendem `N`/`n` und Leerraum
(`"N589"`, `" n 12 "`, `"123"`); Ausgabe von N-Referenzen als `"N<zahl>"`
(`N_ref`). Ungültiger Text ⇒ `AddressError("invalid N reference '…'")`,
`0` ⇒ `ValueError`.

### 2.9 Geometrie der Layouts (logische Abbildungen über der Zelladresse)

Layouts ordnen gültigen Zellen logische Records und Felder zu. Die folgenden
Abbildungen sind in `Db::logical_address_in` (Vorwärts) und
`Db::interpret_layout_address` (Rückwärts) implementiert; sie verändern die
Zellidentität nicht.

| Layout | Vorwärts: Objekt `o`, Feld `f` → Zelle | Rückwärts: Zelle `(R, u, v)` → Record | Anker |
|---|---|---|---|
| CLASSIC | `(R, u = o, v = v_f)`; ohne Feld `v = 1` | `record = u`, Feld = `v` | `v = 1` |
| COLUMN | `(R, u = u_f, v = o)`; ohne Feld `u = anchor_u` (Standard: kleinstes Feld-`u`) | `record = v`, Feld = `u` | `u = anchor_u` |
| PACKED (`u_p`, `span`) | `anchor = (o − 1)·span + 1`, `(R, u = u_p, v = anchor + offset_f)`; `anchor > T` oder `v > T` ⇒ `RealmLimitError` | `record = ⌊(v − 1)/span⌋ + 1`, `offset = (v − 1) mod span` | `offset = 0` |
| CUSTOM | Plugin `logical_address` | Plugin `describe` | Plugin |

Randbedingungen aus `create_layout`: PACKED `1 ≤ span ≤ T` (Standard `10^8`),
`0 ≤ offset < span`, `u_p` im u-Bereich des Layouts; COLUMN-Felder mit
eindeutigem `u` im u-Bereich.

**Kanonischer `record_N`** (`record_n_nat`): `N` der Ankerzelle des Records
auf derselben Spur, unabhängig davon, ob der Anker belegt ist.

**Rechenbeispiele (mit der Engine geprüft):**

| Layout | Anfrage | Rechnung | Ergebnis |
|---|---|---|---|
| CLASSIC, Realm 1 | `record_n_for_address(1, 4, 7)` | Anker `(1, 4, 1, NL)`: `G = 5`, `25 − 4` | `N21` |
| CLASSIC, Realm 1 | `get_n(44, mode="cell")` | `44 → (1, 5, 2, NL)`, Anker `(1, 5, 1)`: `G = 6`, `36 − 5` | `record_N = N31` |
| COLUMN, Realm 2, Felder `u = 3` (a), `u = 5` (b) | Objekt 9, Feld `b` | `(2, 5, 9, NL)`: `V = T + 9`, `G = T + 14`, `N = G² − 5` | `N = 10^200 + 28·10^100 + 191` |
| COLUMN, dto. | Anker Objekt 9 | `anchor_u = 3` ⇒ `(2, 3, 9)` | `u = 3, v = 9` |
| PACKED, Realm 3, `u = 2`, `span = 8` | Objekt 4, Feld mit `offset = 5` | `anchor = 3·8 + 1 = 25`, `v = 30` | `(3, 2, 30)` |
| PACKED, dto. | Interpretation von `v = 30` | `⌊29/8⌋ + 1 = 4`, `29 mod 8 = 5` | `record = 4, offset = 5, anchor = False` |

### 2.10 Routing-Mathematik (Gewinnerregel)

Für `(R, u)` werden alle Layouts des Realms mit `u_start ≤ u ≤ u_end`
(`u_end = None` = unbegrenzt) betrachtet. Sortierschlüssel
(`LayoutDef::sort_cmp`, größer gewinnt):

```text
(priority,  −width,  layout_id)
width = u_end − u_start      bzw. 10^200 für unbegrenzte Layouts
```

### 2.11 Mathematik der MATH_MAP-Schicht

MATH-Layouts (Abschnitt 13) berechnen aus Eingaben `(x1, …, xk)` ganzzahlige
Werte `u = f_u(x)`, `V = f_V(x)` und `side = f_side(x)` und zerlegen `V` mit
denselben Formeln wie Abschnitt 2.3, jedoch über `BigInt` mit wählbarer
Spannweite (`split_global_v(V, realm_span)`, `join_global_v(R, v, realm_span)`
in `mathexpr.rs`; in der Datenbank immer `T = 10^100`):

```text
Voraussetzungen:  u > 0, V > 0 (sonst MathDomainError), side ∈ {0, 1}
(R_log, v) = (⌊(V−1)/T⌋ + 1, ((V−1) mod T) + 1)        (div_mod_floor)
join:       V = (R_log − 1)·T + v      mit R_log ≥ 1, 1 ≤ v ≤ T
```

Beispiel: `u = x`, `V = x·10^100 + 7`, `side = NR`, Eingabe `x = 3` ⇒
`V = 3·T + 7` ⇒ `R_log = 4`, `v = 7`, `side = 1`; `join_global_v(4, 7) = 3·T + 7`.

Der logische Realm wird über die RealmMap auf einen physischen Realm
abgebildet (`physical = physical_start + (R_log − logical_start)` je Extent);
gespeichert und adressiert wird die Zelle `(R_phys, u, v, side)`. Das `N`
einer MATH-Zelle ist daher `N(R_phys, u, v, side)`.

### 2.12 Abgeleitete Zahlenfolgen in der Abfrageschicht

* **Provider `n_sequence`** (`provider_n_sequence`): durchläuft
  `N = start, start+step, …, stop` (`step ≠ 0`), invertiert jede Zahl mit
  `n_to_address` und **überspringt** GZ-/N0-Zahlen (mit `strict=True`
  stattdessen `AddressError`); mit Realm-Kontext werden nur Zellen dieses
  Realms übernommen.
  Beispiel `start = 550, stop = 560` in Realm 1: 552 (N0) entfällt, es
  bleiben 10 Zellen. In der Record-Domäne wird jede Zelle auf ihren
  `record_N` abgebildet, z. B. `554 → (1, 22, 2, NL) → Anker (1, 22, 1)`:
  `G = 23`, `529 − 22 = 507`; `550 → (1, 2, 21, NR) → Anker (1, 2, 1, NR)`:
  `9 + 1 = 10`. Ergebnis: `N5, N10, N273, N307, N343, N381, N421, N463, N507, N553`.
* **Provider `formula`** (`provider_formula`): kompiliert `u` und `V` (oder
  ein lokales `v`, das zu `V = (R−1)·T + v` ergänzt wird) als MATH-Ausdrücke
  über den Parameter `k = start … stop` und liefert
  `N(R_log, u, v, side)` – hier wird **keine** RealmMap angewandt.
* **Provider `math_layout_inputs`**: wendet MathMap und RealmMap eines
  MATH-Layouts an und liefert `N(R_phys, u, v, side)`.

### 2.13 Prinzip versus Implementierungsentscheidung

| Aussage | Art |
|---|---|
| Zellidentität `(R, u, v, lane)`; `N` ist abgeleitet | mathematisches Prinzip |
| Formeln für `V`, `G`, `N`, Ausschluss von GZ/N0, Umkehrung über `isqrt` | mathematisches Prinzip |
| `T = 10^100`, `NL = 0`, `NR = 1` | feste Systemkonstanten (R0,u1,v9), nicht konfigurierbar |
| Stationskennzahlen (`station`, `stufe` …) | berechnete Zusatzinformation, nicht Teil der Identität |
| Layoutgeometrien und Gewinnerregel | logische Interpretationsschicht |
| MATH: Eingaben → `(u, V, side)` → logischer → physischer Realm | Anwendungsschicht; die physische Realmzuordnung ist eine Implementierungsentscheidung (exklusive, frei geplante Realms) |
| strukturelle Blockkodierung, kein N-Index | Implementierungsentscheidung |
| `u64`/`u128`-Schnellpfade, `BigUint`-Ausweichpfad | Implementierungsentscheidung (Performance) |

> **Hinweis zu Nutzdaten:** Ein Schemafeld namens `N` (z. B. im
> Benchmarkprogramm `bench_native` oder im Beispiel in Abschnitt 16.9) ist ein
> gewöhnlicher Anwendungswert, nicht die Zelladresse.

---

## 3. Umsetzung der Mathematik im Rust-Kern

### 3.1 Der Zahltyp `Nat` (`nat.rs`)

```rust
pub enum Nat {
    S(u64),            // Werte 0 … u64::MAX direkt
    B(Box<BigUint>),   // ausschließlich Werte > u64::MAX
}
```

* **Invariante:** `B` enthält nur Werte `> u64::MAX`. Alle Konstruktoren
  (`From<u128>`, `From<BigUint>`, Arithmetik) normalisieren entsprechend.
  Deshalb sind die abgeleiteten `Eq`, `Ord`, `Hash` numerisch korrekt
  (`S(_) < B(_)` gilt immer).
* Operationen: `add`, `add_u64`, `checked_sub`, `sub_u64`, `mul`, `div_rem`,
  `pow10`, `parse_decimal`; jeweils mit `u64`-Schnellpfad und
  `BigUint`-Ausweichpfad.
* `T = 10^100` wird einmal als `Nat::pow10(100)` berechnet und in einem
  `OnceLock` gehalten (`realm_span()`); ebenso `10^200` als Breite
  unbegrenzter Layouts (`unbounded_width()`).

### 3.2 Rechenpfade der Adressfunktionen (`address.rs`)

| Funktion | Ablauf im Code |
|---|---|
| `require_positive(name, x)` | `x = 0` ⇒ `ValueError("<name> must be > 0")` (negative Python-Zahlen bildet die PyO3-Schicht auf 0 ab, damit derselbe Text entsteht) |
| `validate_local_v(v, R)` | `v = 0` ⇒ `RealmLimitError`; nur wenn `v` als `Nat::B` vorliegt, kann `v > T` gelten – dann `RealmLimitError` (Werte ≤ `u64::MAX` sind stets `< T`) |
| `global_v_unchecked(R, v)` | `R = 1` ⇒ `V = v` (keine Multiplikation), sonst `(R−1)·T + v` über `BigUint` |
| `split_global_v(V)` | `V` als `Nat::S` ⇒ sofort `(1, V)`; sonst `div_rem(V − 1, T)` |
| `n_pair(u, V)` | `G = u + V`; wenn `u`, `V` als `u64` vorliegen und `G < 2^63`: `G²` in `u128`, `N_NL = G² − u`, `N_NR = G² + V` in `u128`; sonst `BigUint` |
| `dynadron_address` | Validierung, `V`, `n_pair`, Kennzahlen über `div_rem` |
| `dynadron_n` | Validierung, `V`, `n_pair`, Auswahl nach `lane` |
| `isqrt(N)` | `Nat::S`: Startwert `⌊√N⌋` über `f64`, danach exakte Korrektur in `u128` (`while r² > N: r −= 1; while (r+1)² ≤ N: r += 1`); `Nat::B`: `BigUint::sqrt` |
| `n_to_address(N)` | Algorithmus 2.6, abschließende Kontrolle `cell_address(...).n == N` |

**Warum der `u128`-Pfad sicher ist:** Für `G < 2^63` gilt `G² < 2^126`, und
`G² + V < 2^126 + 2^63 < 2^128`; ein Überlauf ist ausgeschlossen. Für
`R ≥ 2` ist `V ≥ T + 1 > 2^64`, `V` ist also ein `Nat::B`, und der
`BigUint`-Pfad wird verwendet.

**Warum die Fließkomma-Startschätzung exakt wird:** Die Schätzung dient nur
als Startwert; die beiden Korrekturschleifen stellen `r² ≤ N < (r+1)²` in
exakter `u128`-Arithmetik her.

### 3.3 Durchgerechnete Beispiele durch den Rust-Core

**Beispiel A – `dynadron_n(1, 11, 13, NR)`:**

```text
require_positive(R=1), require_positive(u=11), validate_local_v(13)   ok
global_v_unchecked: R = 1  ⇒  V = 13                                 (Nat::S)
n_pair: u, V als u64, G = 24 < 2^63  ⇒  u128-Pfad
        G² = 576, NL = 576 − 11 = 565, NR = 576 + 13 = 589
Ergebnis (lane = NR): 589
```

**Beispiel B – `n_to_address(589)`:**

```text
isqrt(589): f64-Schätzung 24, Korrektur: 24² = 576 ≤ 589 < 625  ⇒  m = 24
d = 589 − 576 = 13;  1 ≤ 13 < 24  ⇒  NR, u = 24 − 13 = 11, V = 13
split_global_v(13): Nat::S  ⇒  (R = 1, v = 13)
Kontrolle: cell_address(1, 11, 13, NR).n = 589 = N   ⇒   (1, 11, 13, NR)
```

**Beispiel C – `dynadron_n(23, 14, 744, NL)`:**

```text
global_v_unchecked: R = 23  ⇒  V = 22·T + 744          (Nat::B, BigUint-Pfad)
n_pair: V ist Nat::B  ⇒  BigUint: G = 22·T + 758, N = G² − 14
N hat 203 Dezimalstellen
n_to_address(N): isqrt über BigUint::sqrt ⇒ m = 22·T + 757;
  d = N − m² = 2m + 1 − 14 = 2m − 13 > m  ⇒  NL, G = m + 1, u = G² − N = 14,
  V = G − 14 = 22·T + 744;  split_global_v(V) ⇒ (23, 744)
```

(Testfall `r23_u14_v744` in `address.rs`, zusätzlich NR-Rückrechnung.)

**Beispiel D – Grenzzahl `n_to_address(552)`:** `isqrt(552) = 23`,
`d = 552 − 529 = 23 = m` ⇒ `AddressError("N552 is an N0 point, not an NHyDB storage cell")`.

### 3.4 Physische Kodierung der Koordinaten

Koordinaten werden **strukturell** und blockweise kodiert, nie als
vollständige Tupel und nie als `N` je Zelle:

| Koordinate | Dense-Block (`DATA_DENSE`) | Sparse-Block (`SPARSE_CELLS`) |
|---|---|---|
| `R` | VarUInt im Präfix | VarUInt im Präfix |
| `lane` | Bit 0 der Blockflags | Bit 0 der Blockflags |
| `u` | `u_start` im Präfix; Record `i` hat `u = u_start + i` | `u_start`, `u_end` im Präfix; je Gruppe `Δu` (erste Gruppe `Δu = 0`) |
| `v` | implizit durch die Feldposition im Shape | je Zelle `Δv` zum Vorgänger (Start 0), streng aufsteigend |

Die Sparse-Zelle speichert `Δv` gemeinsam mit drei Flagbits als **eine**
VarUInt: `key = (Δv << 3) | flags` mit `flags = tomb | selbsttypisiert<<1 |
null<<2`. Für `Δv < 2^60` wird in `u64` gerechnet, sonst über `BigUint`.

Beispiel: Zellen `v = 2` und `v = 9` einer u-Gruppe, beide deklariert, nicht
null: Schlüssel `(2 << 3) | 0 = 16` und `((9 − 2) << 3) | 0 = 56`.

### 3.5 VarUInt und VarInt

```text
VarUInt:  n = Σ b_i·128^i, Byte i = b_i | 0x80 (außer letztem Byte)
VarInt:   z = 2n (n ≥ 0),  z = −2n − 1 (n < 0);  danach VarUInt(z)
          Rückabbildung: z gerade ⇒ n = z/2, z ungerade ⇒ n = −(z+1)/2
```

Mit der Engine geprüfte Kodierungen (`enc_vu`, `enc_vi`, hexadezimal):

| Wert | VarUInt | | Wert | VarInt |
|---:|---|---|---:|---|
| 0 | `00` | | 0 | `00` |
| 1 | `01` | | −1 | `01` |
| 127 | `7f` | | 1 | `02` |
| 128 | `8001` | | −3 | `05` |
| 300 | `ac02` | | 3 | `06` |
| 2^64 | `80808080808080808002` | | | |

Implementierung: `put_vu64` für `u64`, `put_vu_big` über
`BigUint::to_radix_le(128)`; `get_vu64` dekodiert bis 64 Bit in einem
`u128`-Akkumulator und fällt sonst auf `get_vu` (beliebige Größe) zurück.
`get_vu_u64` wird für physische Felder (Längen, Zähler, IDs) verwendet und
lehnt Werte > `u64::MAX` mit `CorruptionError("…: value too large")` ab.

### 3.6 Physische Grenze großer Koordinaten

`get_vu` bricht ab, sobald die Schiebeweite 16 384 Bit überschreitet
(`CorruptionError("VarUInt implausibly large")`). Eine einzelne in einer
Datei gespeicherte VarUInt kann damit höchstens 2 341 Byte (≈ 16 387 Bit)
lang sein. Das ist eine **Schutzgrenze des Dekoders** gegen manipulierte
Dateien, keine Einschränkung der Adressmathematik selbst; Koordinaten der
Größenordnung `T = 10^100` (333 Bit)
liegen weit darunter (das 203-stellige `N` des Beispiels C hat rund 672 Bit).

### 3.7 Feste Systemkonstanten je Datenbank

Jede neu angelegte Datenbank schreibt in R0,u1 die Zelle
`v9 = {"lane_NL": 0, "lane_NR": 1, "realm_span": 10**100}`. `check_record`
(`db/system.rs`) prüft beim Öffnen, dass v9 – falls vorhanden – exakt diesem
Wert entspricht und `realm_span` numerisch gleich `realm_span()` ist; sonst
`CorruptionError("R0,u1,v9: fixed system constants violated …")`. Fehlt v9, gilt
derselbe Wert.

### 3.8 Prüfung der Mathematik

| Test | Inhalt |
|---|---|
| `address.rs::examples_from_docs` | 589 → `(1,11,13,NR)`, NL-Partner 565, 391 → `(1,9,11,NL)`, Ablehnung von 552, 576, 600, 4; Randzellen 553/575 (NL) und 577/599 (NR) mit `G = 24`; `(1,1,1,NL) = 3` |
| `address.rs::r23_u14_v744` | Vorwärts = `(22·T+758)² − 14`, 203 Stellen, Rückrechnung NL und NR |
| `address.rs::realm_edges` | `split_global_v(T) = (1, T)`, `split_global_v(T+1) = (2, 1)`, Roundtrips für sehr große `R` und `u` auf beiden Spuren, `v`-Grenzen, Null-Koordinaten |
| `address.rs::exhaustive_small_range` | alle `N` in `[1, 200 000)`: entweder GZ/N0 oder exakt invertierbar; > 199 000 gültige Zellen |
| `address.rs::parse_refs` | Textformen von N-Referenzen |
| `nat.rs` (Unit-Tests) | VarUInt/VarInt inkl. `i64::MIN`, abgeschnittene Eingaben |
| `tests/python/test_math.py` | Ablehnung von GZ-/N0-Zahlen, Rechenbeispiel `(23, 14, 744)` |
| `crates/nhydb-core/tests/sparse_access.rs` | Sparse-Zugriff mit `u = 2^70` und `v ≥ 10^40` gegen ein unabhängiges Modell |

---

## 4. Architektur und Komponenten

### 4.1 Schichtenmodell

```mermaid
flowchart TB
    subgraph PY["Python-Paket nhydb"]
        API["nhydb (NHyDB, Funktionen, Ausnahmen)"]
        SUB["nhydb.kernel · nhydb.index · nhydb.math · nhydb.mathlayout · nhydb.query"]
        CLI["CLI: python -m nhydb · python -m nhydb.query"]
    end
    subgraph PYO3["crates/nhydb-py (PyO3-Modul nhydb._native)"]
        CONV["Konvertierung Python ↔ Val/Nat"]
        NPIO["NumPy-Buffer-Protokoll"]
        PLUG["Plugin- und Provider-Brücke"]
        ITER["ResultIterator"]
    end
    subgraph CORE["crates/nhydb-core (reines Rust)"]
        ADDR["address · nat"]
        SCHEMA["schema · codec · val"]
        DB["db::Db (Öffnen, Commit, Blöcke)"]
        STORE["dense · sparse · vindex · interval"]
        LAYOUT["layout (Routing, Evolution, Interpretation)"]
        READ["read · materialize · cursor · columns"]
        IDX["index · textindex · setops"]
        MATH["mathexpr · mathlayout · db::math"]
        Q["query · sql"]
        MAINT["maint (verify, health, stats, repair, compact)"]
        SYS["db::system · crypto"]
        PLAT["platform · storage · format · config · zlib · unicode"]
    end
    FILES[("X.nhydb · X.nhydb.layout.enc · X.nhydb.layout.key · X.nhydb.lock")]
    API --> PYO3
    SUB --> PYO3
    CLI --> API
    PYO3 --> CORE
    DB --> STORE
    DB --> SYS
    LAYOUT --> STORE
    READ --> LAYOUT
    IDX --> READ
    Q --> IDX
    Q --> MATH
    MATH --> STORE
    MAINT --> DB
    STORE --> PLAT
    SYS --> PLAT
    PLAT --> FILES
```

**Architekturentscheidung:** Der Kern `nhydb-core` hat keine
Python-Abhängigkeit und ist mit `cargo test -p nhydb-core` eigenständig
testbar. Adressmathematik, Kodierung, Routing und Commit liegen
ausschließlich im Kern; die PyO3-Schicht konvertiert nur Typen, gibt den GIL
für rein native Abschnitte frei und überbrückt Python-Plugins.

### 4.2 Module des Rust-Kerns

Zeilenzahlen: physische Zeilen der Quelldateien in NHyDB 0.27.0 (`wc -l`,
einschließlich Kommentaren und Unit-Tests); sie dienen nur der Orientierung.

| Modul | Zeilen | Aufgabe |
|---|---:|---|
| `lib.rs` | 50 | Crate-Wurzel, öffentliche Re-Exporte |
| `error.rs` | 155 | Fehlertyp `Error` (Abbildung auf die Python-Ausnahmen) |
| `nat.rs` | 458 | Zahltyp `Nat`, VarUInt/VarInt-Kodierung |
| `val.rs` | 927 | dynamischer Werttyp `Val` (None/Bool/Int/Big/Float/Str/Bytes/List/Map), Python-kompatibles JSON |
| `address.rs` | 411 | Adressmathematik (Abschnitt 3) |
| `codec.rs` | 479 | 15 Datentypen, `FieldDef`, Wert-Kodierung/-Dekodierung |
| `schema.rs` | 585 | `ShapeDef`, `RealmDef`, `ExtensionDef`, `LayoutDef`, `IndexDef` |
| `format.rs` | 217 | Superblock, Blockheader, Blocktypen, Konstanten |
| `storage.rs` | 147 | Append-Puffer, positionsbasiertes Lesen, Sync |
| `platform.rs` | 425 | OS-Schicht: Durability, Verzeichnis-Sync, atomarer Austausch, Sperren |
| `config.rs` | 242 | `nhydb_config.toml` |
| `crypto.rs` | 161 | AES-256-GCM, HMAC-SHA256, SHA-256, Base64, UUIDv4 |
| `unicode.rs` + `unicode_tables.rs` | 180 + 17 (generierte Tabellen, sehr lange Zeilen) | CPython-exakte Zeichenklassen und `casefold` (Unicode 16.0.0) |
| `zlib.rs` | 53 | zlib-Kompression der Postings |
| `plugin.rs` | 46 | Trait `LayoutPlugin` für CUSTOM-Layouts |
| `setops.rs` | 96 | sortierte Mengenoperationen (galoppierender Schnitt) |
| `textindex.rs` | 625 | Tokenizer, N-Gramme, FULLTEXT-Codecs, Phrase, BM25 |
| `mathexpr.rs` | 1 942 | MATH-Ausdruckssprache, `MathMap`, `MathMapLayoutSpec`, `dry_run` |
| `mathlayout.rs` | 546 | Realmplaner für MATH-Layouts |
| `query.rs` | 2 397 | QueryEngine: Ausdrucksbaum, Planer, Auswertung, Aggregation, Suche |
| `sql.rs` | 1 252 | NHySQL: Lexer, Parser, Compiler |
| `db/mod.rs` | 1 197 | `Db`: Zustand, Öffnen, Commit, Blöcke, Open-Scan |
| `db/system.rs` | 397 | R0,u1, Schlüssel-/Plan-Dateien, Schema-Snapshot |
| `db/schema_ops.rs` | 427 | Realms, Shapes, Extensions, Indexdefinitionen |
| `db/layout.rs` | 1 451 | Layouts, SpaceMaps, Routing, Evolution, logische Adressen |
| `db/dense.rs` | 620 | Dense-BASE-Records |
| `db/sparse.rs` | 555 | Sparse-Zellen |
| `db/vindex.rs` | 554 | v-Index je u-Gruppe (gezielter Sparse-Zugriff) |
| `db/interval.rs` | 152 | Intervallindex mit Präfixmaximum |
| `db/read.rs` | 828 | Lese-APIs, geometrisches GET |
| `db/materialize.rs` | 374 | Iteration materialisierter Zellen, `record_N` |
| `db/cursor.rs` | 330 | Lazy-Cursor für `range`, `select`, `find`, Sparse-Punkte |
| `db/columns.rs` | 489 | spaltenweises Schreiben/Lesen (NumPy-Pfade) |
| `db/index.rs` | 1 155 | Indexaufbau, Verzeichnisse, Kandidaten |
| `db/math.rs` | 553 | MATH-API im Kern, Rollback |
| `db/maint.rs` | 1 060 | Checkpoint, Verify, Health, Stats, Repair, Kompaktierung |

### 4.3 PyO3-Schicht `nhydb-py`

| Datei | Aufgabe |
|---|---|
| `lib.rs` | Klasse `NHyDB`, `LayoutDef`, `IndexDef`, Modulfunktionen, Registrierung |
| `conv.rs` | Konvertierung Python ↔ `Val`/`Nat` (beliebig große `int`) |
| `exceptions.rs` | Ausnahmehierarchie |
| `numpy_io.rs` | Buffer-Protokoll für NumPy-Arrays, Ausgabe über `numpy.frombuffer` |
| `iter.rs` | `ResultIterator` (lazy, blockweise) |
| `pyplugin.rs` | Brücke für Python-CUSTOM-Plugins (Weak-Referenz auf das DB-Objekt) |
| `kernel.rs` | zustandslose Kernelfunktionen für `nhydb.kernel` |
| `textapi.rs` | Funktionen für `nhydb.index` |
| `mathapi.rs` | Klassen für `nhydb.math`/`nhydb.mathlayout`, MATH-Methoden |
| `queryapi.rs` | native `QueryEngine`, SQL, Provider-Brücke |

### 4.4 Externe Abhängigkeiten

Versionen laut `Cargo.lock` von NHyDB 0.27.0:

| Crate | Version | Verwendung |
|---|---|---|
| `num-bigint` / `num-integer` / `num-traits` | 0.4.8 / 0.1.47 / 0.2.19 | exakte Ganzzahlarithmetik |
| `crc32fast` | 1.5.2 | CRC32 für Superblöcke, Blockheader, Payloads |
| `miniz_oxide` | 0.8.9 | zlib-Dekompression; Kompression unter Windows |
| `aes-gcm` | 0.11.1 | AEAD-Verschlüsselung des Layoutplans |
| `sha2` | 0.11.1 | SHA-256 (Snapshot-Hash, Fingerprints, MAC-Schlüsselableitung) |
| `hmac` | 0.13.0 | HMAC-SHA256 über den Planumschlag |
| `base64` | 0.23.1 | Kodierung von Schlüssel, Nonce und Ciphertext |
| `getrandom` | 0.4.3 | Zufallszahlen des Betriebssystems (Schlüssel, Nonces, UUID) |
| `toml` | 1.1.8 | Konfigurationsdatei |
| `libc` (nur Unix) | 0.2.190 | `fsync`, `fcntl(F_FULLFSYNC)` |
| `windows-sys` (nur Windows) | 0.61.2 | `FlushFileBuffers`, `MoveFileExW` |
| `pyo3` (nur `nhydb-py`) | 0.29.3 | Python-Anbindung, Feature `num-bigint` |
| System-`libz` (Unix, per FFI) | – | zlib-Kompression bytegleich zu CPython |

Es wird keine eigene Kryptografie implementiert; JSON wird mit einem eigenen,
Python-kompatiblen Codec in `val.rs` erzeugt (kein `serde`).

### 4.5 Release-Profil

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "unwind"
```

---

## 5. Datenmodell: Realms, Shapes, Extensions, Layouts

### 5.1 Begriffsübersicht

```mermaid
flowchart LR
    R["Realm R<br/>(logischer Adressbereich)"] --> S["Shape<br/>(BASE-Felder v=1..k)"]
    R --> E["Extension<br/>(deklariertes Feld an v)"]
    R --> L["Layout<br/>(CLASSIC/COLUMN/PACKED/CUSTOM,<br/>u-Bereich, Priorität)"]
    R --> I["IndexDef<br/>(VALUE/TOKEN/TRIGRAM/FULLTEXT)"]
    L -- "shape_id (CLASSIC)" --> S
    M["MATH-Layout<br/>(MathLayoutState)"] -- "besitzt exklusiv" --> R
```

| Objekt | Rust-Typ | Persistenz | Wichtige Felder |
|---|---|---|---|
| Realm | `RealmDef` | externer Layoutplan | `realm_id`, `name`, `description`, `default_shape_id`, `base_reserve_to` |
| Shape | `ShapeDef` | externer Layoutplan | `shape_id`, `realm_id`, `name`, `fields: Vec<FieldDef>` |
| Feld | `FieldDef` | Teil von Shape/Extension | `v`, `name`, `typ`, `enum_map`, `description` |
| Extension | `ExtensionDef` | externer Layoutplan | `realm_id`, `v`, `typ`, `name`, `enum_map` |
| Layout | `LayoutDef` | externer Layoutplan | `layout_id`, `realm_id`, `kind`, `u_start`, `u_end`, `priority`, `shape_id`, `params`, `spacemap`, `plugin` |
| Index | `IndexDef` | externer Layoutplan | `index_id`, `realm_id`, `name`, `kind`, `source_v`, `options` |
| MATH-Zustand | `MathLayoutState` | externer Layoutplan | Spezifikation, RealmMap, `payload_type`, `frozen` |

Alle Schemaobjekte liegen in Format 0.16 **nicht** in der Datenbankdatei,
sondern verschlüsselt im externen Layoutplan (Abschnitt 8). In der
Datenbankdatei stehen nur Daten-, Index-, Lösch-, Checkpoint-, Konfigurations-
und SYSTEM-Blöcke.

### 5.2 Dense und Sparse

* **Dense-BASE-Record:** ein zusammenhängender Datensatz an `(R, u, lane)` mit
  den Feldern eines Shapes in Feldreihenfolge (`v = 1 … k`). Er wird
  blockweise für zusammenhängende `u`-Bereiche geschrieben (`put_dense_batch`).
* **Sparse-Zelle:** eine einzelne Zelle `(R, u, v, lane)` mit Wert, Null oder
  Tombstone. Sparse-Zellen ergänzen oder überlagern Dense-Records
  (Extensions, Einzelwerte, COLUMN-/PACKED-/CUSTOM-Objekte, MATH-Zellen).

Drei Zustände sind streng getrennt:

| Zustand | Bedeutung | Kodierung |
|---|---|---|
| fehlend | Zelle wurde nie geschrieben | kein Eintrag |
| Null | Zelle existiert mit Wert `None` | Sparse-Flag `4` |
| Tombstone | Zelle wurde gelöscht | Sparse-Flag `1` |

Der jeweils **neueste** Zustand (höchste Block-ID) gewinnt.

### 5.3 Extensions und BASE-Grenze

Extensions sind deklarierte Zusatzfelder eines Realms an einem `v` oberhalb
der BASE-Felder. Sie werden als Sparse-Zellen gespeichert (ein optionaler
EXT-Bereich `0xEA` in Dense-Records wird beim Lesen unterstützt, von der
Engine aber nicht geschrieben). `define_extension`, `put_extensions`, `put_extension_list` und
`put_extension_range` arbeiten auf dieser Ebene. `base_reserve_to` reserviert
einen `v`-Bereich für künftige BASE-Felder.

---

## 6. Datentypen und Wertkodierung

### 6.1 Die 15 Feldtypen

| Typcode | Name | Kodierung | Wertebereich / Hinweise |
|---:|---|---|---|
| 1 | `u8` | 1 Byte LE | 0 … 255 |
| 2 | `u16` | 2 Byte LE | 0 … 65 535 |
| 3 | `u32` | 4 Byte LE | 0 … 2³²−1 |
| 4 | `u64` | 8 Byte LE | 0 … 2⁶⁴−1 |
| 5 | `i8` | 1 Byte LE | −128 … 127 |
| 6 | `i16` | 2 Byte LE | |
| 7 | `i32` | 4 Byte LE | |
| 8 | `i64` | 8 Byte LE | |
| 9 | `f32` | 4 Byte IEEE 754 LE | endliche Werte, die als `f32` unendlich würden, werden abgewiesen |
| 10 | `f64` | 8 Byte IEEE 754 LE | |
| 11 | `bool` | 1 Byte | Python-Wahrheitswert |
| 12 | `varuint` | VarUInt | beliebig groß, ≥ 0 |
| 13 | `varint` | VarInt (ZigZag + VarUInt) | beliebig groß |
| 14 | `utf8` | `vu(Länge)` + UTF-8-Bytes | Nicht-Strings werden wie Python-`str()` umgewandelt |
| 15 | `bytes` | `vu(Länge)` + Bytes | |

Die Typcodes sind als `nhydb.TYPE_CODES` abrufbar.

### 6.2 VarUInt und VarInt

* **VarUInt (`vu`):** Basis 128, sieben Datenbits je Byte, Little-Endian,
  höchstes Bit = Fortsetzung. Beliebig groß (für `Nat::B` über `BigUint`).
* **VarInt (`vi`):** ZigZag-Abbildung `n ≥ 0 → 2n`, `n < 0 → −2n−1`,
  anschließend VarUInt.
* Dekodierfehler ergeben `CorruptionError` mit festen Texten
  (`VarUInt ends prematurely`, `VarUInt implausibly large`,
  `length value ends beyond the payload`, `fixed-width value incomplete`).

### 6.3 Enums

Ein Feld kann eine Enum-Abbildung `Label → Code` (Codes als `i64`) tragen.
Beim Schreiben werden Labels in Codes übersetzt, beim Lesen zurück. Bei
doppelten Codes gewinnt – wie beim Python-Dict-Comprehension – der letzte
Eintrag. Unbekannte Labels ergeben
`ValueError("<feld>: unknown enum value '<x>'")`.

### 6.4 Selbsttypisierte Sparse-Zellen

Sparse-Zellen, für die kein Typ aus Shape, Extension oder Layout ableitbar
ist, oder die mit explizitem `type=` geschrieben werden, speichern ihren
Typcode als zusätzliches Byte („selbsttypisiert“). CUSTOM-Layoutzellen und
MATH-Zellen sind stets selbsttypisiert. Ist kein Typ ableitbar und keiner
angegeben, wird mit `SchemaError("…: cannot derive a data type; specify type=...")`
abgelehnt.

### 6.5 Der Werttyp `Val`

Zellwerte, Layoutparameter und API-Ergebnisse werden im Kern als `Val`
repräsentiert: `None`, `Bool`, `Int(i64)`, `Big(BigInt)`, `Float(f64)`,
`Str`, `Bytes`, `List`, `Map` (geordnete Schlüssel). `Val` implementiert die
**Python-Gleichheits- und Ordnungssemantik** (z. B. `1 == 1.0 == True`), die
für Aggregation, Gruppierung und Sortierung der Abfrageschicht nötig ist,
sowie einen JSON-Codec, der bytegleich zu
`json.dumps(…, sort_keys=True, separators=(",", ":"), ensure_ascii=False)`
serialisiert (für Fingerprints zusätzlich ein `ensure_ascii=True`-Modus).

---

## 7. Speicherorganisation und Dateiformat 0.16

Die Engine schreibt und liest das Dateiformat mit der Kennung `NHYDB016`
(Formatrevision 16, `FILE_FORMAT_VERSION = "0.16"`). Ältere Formatrevisionen
werden nicht gelesen; einen Konverter gibt es nicht. Alle Ganzzahlen sind Little-Endian.

### 7.1 Dateien einer Datenbank

| Datei | Inhalt | Hinweis |
|---|---|---|
| `X.nhydb` | Container (Superblöcke, Blöcke) | |
| `X.nhydb.layout.enc` | verschlüsselter Layoutplan (1–2 Schema-Generationen) | Pfad wählbar |
| `X.nhydb.layout.key` | 32-Byte-Schlüssel (JSON), Unix-Rechte `0600` | **separat sichern**; ohne ihn ist die DB nicht zu öffnen |
| `X.nhydb.lock` | Sperrdatei für den exklusiven Schreibzugriff (leer, bleibt bestehen) | |

Pfade von Plan und Schlüssel sind über `layout_path`/`key_path` bzw. die
Konfiguration wählbar; R0,u1 speichert nur die **Dateinamen**, keine Pfade.

### 7.2 Gesamtlayout der Datei

```text
Offset 0      Superblock A (4096 Byte)
Offset 4096   Superblock B (4096 Byte)
Offset 8192   Block 1: SYSTEM (R0,u1, Generation 0)
              Block 2: DB_CONFIG
              Block …: DATA_DENSE | SPARSE_CELLS | DELETE | SECONDARY | CHECKPOINT | SYSTEM | DB_CONFIG
              ── Commit-Grenze last_valid_end ──
              (optional: unkommittierter Tail, wird beim Öffnen abgeschnitten)
```

### 7.3 Superblock

| Offset | Typ | Inhalt |
|---:|---|---|
| 0 | 8 Byte | `NHYDB016` |
| 8 | u16 | Major `0` |
| 10 | u16 | Minor `16` |
| 12 | u32 | Superblockgröße `4096` |
| 16 | u64 | `generation` |
| 24 | u64 | `checkpoint_offset` |
| 32 | u64 | `last_valid_end` (Commit-Grenze) |
| 40 | u64 | `next_block_id` |
| 48 | u64 | `created_ns` |
| 56 | u64 | `updated_ns` |
| 64 | u32 | `flags` |
| 4092 | u32 | CRC32 über Bytes 0 … 4091 |

Beim Öffnen gewinnt der gültige Superblock mit der höchsten Generation. Ein
Commit schreibt stets den **anderen** Slot.

### 7.4 Blockheader

32 Byte, Struktur `<4sBBHQQII`:

| Feld | Typ | Inhalt |
|---|---|---|
| Magic | 4 Byte | `NHBL` |
| Version | u8 | `16` |
| Typ | u8 | Blocktyp (Tabelle 7.5) |
| Flags | u16 | Bit 0 = Lane NR |
| Block-ID | u64 | fortlaufend |
| Payloadlänge | u64 | |
| Payload-CRC | u32 | CRC32 der Payload |
| Header-CRC | u32 | CRC32 des Headers mit Header-CRC = 0 |

Nach der Payload folgt Null-Padding auf ein Vielfaches von 8 Byte.

### 7.5 Blocktypen

| Typ | Name | Ort | Payload |
|---:|---|---|---|
| 1 | REALM | nur Layoutplan | – |
| 2 | SHAPE | nur Layoutplan | – |
| 3 | INDEX_DEF | nur Layoutplan | – |
| 4 | DATA_DENSE | DB | `D10D vu(realm) vu(shape) vu(u_start) vu(count) u32[count+1] records` |
| 5 | DELETE | DB | `vu(realm) vu(u)` |
| 6 | CHECKPOINT | DB | `CP10` + JSON |
| 7 | SECONDARY | DB | `SI10 vu(index) vu(source_block) vu(entries) vu(dirlen) dir postings` |
| 8 | EXTENSION_DEF | nur Layoutplan | – |
| 9 | LAYOUT_DEF | nur Layoutplan | – |
| 10 | SPARSE_CELLS | DB | `SP10 vu(realm) vu(u_start) vu(u_end) vu(groups) vu(cells) stream` |
| 11 | MATH_LAYOUT_STATE | nur Layoutplan | – |
| 12 | DB_CONFIG | DB | JSON `{"contract":"NHYDB_DB_CONFIG_V015","max_db_size_bytes":N}` |
| 13 | SYSTEM (R0,u1) | DB | JSON `{"contract":"NHYDB027_R0U1_V1","cells":{…}}` |

Ein Layout-Metablock (Typen 1, 2, 3, 8, 9, 11) in der Datenbankdatei ist ein
Fehler; das Öffnen schlägt mit `CorruptionError` fehl (fail closed).

**Implementierungsdetail:** Schemaoperationen rufen intern weiterhin den
Append-Pfad mit diesen Blocktypen auf. `Db::append_block_limit` fängt sie ab,
schreibt nichts in die Datei und setzt stattdessen `layout_dirty`, sodass der
nächste Commit eine neue Plangeneration erzeugt.

### 7.6 Dense-Block

```text
"D10D"
vu(realm) vu(shape_id) vu(u_start) vu(count)
u32[count + 1]          Offsets relativ zum Recordbereich (letzter = Gesamtlänge)
records                 Record i = Bytes [off[i], off[i+1])
```

Ein Record enthält die BASE-Felder in Shape-Reihenfolge (feste Typen LE,
`varuint`/`varint`, `utf8`/`bytes` als `vu(len)` + Bytes, `bool` 1 Byte).
Ein optionaler EXT-Bereich
`0xEA vu(count) (vu(Δv<<1 | has_value) [wert])*` wird beim Lesen
unterstützt, aber nicht geschrieben. Wegen der 32-Bit-Offsets darf der Recordbereich eines
Batches höchstens 4 GiB umfassen
(`ValueError("a DATA batch may contain at most 4 GiB of record data")`).

### 7.7 Sparse-Block

```text
"SP10"
vu(realm) vu(u_start) vu(u_end) vu(groups) vu(cells)
stream:  je u-Gruppe:  vu(Δu) vu(len) record        (erstes Δu = 0)
record:  vu(count), dann je Zelle aufsteigend nach v:
         vu(Δv << 3 | flags)  [u8 typcode, falls selbsttypisiert]  [wert, außer Tombstone/Null]
flags:   1 = Tombstone, 2 = selbsttypisiert, 4 = Null
```

`v` muss je `u` streng aufsteigend sein (`SchemaError`). Schreibvorgänge
gruppieren Zellen in `u`-Fenstern (Standard `u_window = 10 000`, MATH:
`10^8`) zu je einem Block.

### 7.8 SECONDARY-Block (Index)

Verzeichnis-Einträge: `vu(len) JSON-Schlüssel` (Bytes als
`{"__bytes__": hex}`), `vu(rel_offset) vu(comp_len) vu(count) u8(codec)`.
Schlüssel stehen in Python-Sortierordnung (bei gemischten Typen nach `repr`).

| Codec | Inhalt |
|---:|---|
| 4 / 8 | `u`-Werte als u32- bzw. u64-Array |
| 0 | VarUInt-Deltas (für `u ≥ 2^64`) |
| 1 | FULLTEXT-Positionen: `FTP1 vu(n) (vu(Δu) vu(tf) vu(pos0) vu(Δpos)…)*` |
| 2 | FULLTEXT-Dokumentlängen: `FTD1 vu(n) vu(total_tokens) (vu(Δu) vu(len))*`, Schlüssel `"\u0000NHYDB_FULLTEXT_DOCUMENT_LENGTHS"` |

Postings sind zlib-komprimiert (Level 1; Unix: System-`libz`, bytegleich zu
CPython; Windows: `miniz_oxide`, gültiger, aber nicht notwendig bytegleicher
Strom). `count` ist die Dokumentfrequenz bzw. Dokumentanzahl. Verzeichnisse
werden beim Öffnen nicht dekodiert, sondern beim ersten Zugriff.

### 7.9 CHECKPOINT-Block

`CP10` gefolgt von JSON mit `version` (11), `data_max_block_id`, je Realm und
Lane der Segmentanzahl und `max_u` sowie `secondary_source_block`. Ein
Checkpoint wird über `checkpoint()` angehängt und sofort committet. Er macht
frühere Blöcke **nicht** überflüssig; er ist eine Momentaufnahme für
Diagnosezwecke (`health` meldet eine Warnung, wenn keiner existiert).

### 7.10 Speicherbedarf

Gemessene Dateigrößen des Kernbenchmarks (Abschnitt 18.5): 1 Mio.
KQ-Datensätze BASE_ONLY ≈ 23,4 MB (≈ 23,4 Byte je Record bei 11 Feldern), SPARSE_MIX zusätzlich
≈ 9,78 Byte je materialisierter Extension-Zelle. Diese Werte gelten nur für
das jeweilige Benchmark-Schema.

---

## 8. Systemadressraum R0,u1 und externer Layoutplan

### 8.1 Zweck

Format 0.16 trennt **Daten** (in `X.nhydb`) von **Schema und Layout**
(verschlüsselt in `X.nhydb.layout.enc`). Der Systemadressraum R0,u1 in der
Datenbankdatei bindet die Datenbank kryptografisch an genau eine
Plangeneration. Fehlt der Schlüssel, ist der Plan manipuliert oder passt die
Generation nicht, wird das Öffnen verweigert.

> **Abgrenzung:** Realm 0 ist **kein** Dynadron-Adressraum. Für Benutzer ist
> Realm 0 ungültig (`require_positive`). R0,u1 ist eine reservierte
> Systemstruktur mit eigener API (`get_system_metadata`, `get_system_cell`).

### 8.2 Zellen von R0,u1

| Zelle | Inhalt | veränderlich |
|---:|---|---|
| v1 | Datenbank-UUID (v4) | nein |
| v2 | ursprünglicher DB-Dateiname | nein |
| v3 | Dateiname des Layoutplans | nein |
| v4 | Dateiname der Schlüsseldatei | nein |
| v5 | Plangeneration (+1 je SYSTEM-Block) | ja |
| v6 | SHA-256 (hex) des aktiven Schema-Snapshots | ja |
| v7 | `created_ns` | nein |
| v8 | `"AES-256-GCM + HMAC-SHA256"` | nein |
| v9 | `{"lane_NL":0,"lane_NR":1,"realm_span":10**100}` (nur im ersten SYSTEM-Block) | nein |

v9 steht nur im ersten SYSTEM-Block der Kette; Folgegenerationen enthalten
v9 nicht. `get_system_cell(9)` liefert v9 dennoch aus der gemerkten Kette.

### 8.3 Schlüssel- und Plandatei

* **Schlüsseldatei:** JSON
  `{"contract":"NHYDB027_KEY_V1","db_id","db_name","key_b64"}` mit 32
  Zufallsbytes; exklusiv angelegt (`create_new`), Unix-Rechte `0600`,
  Datei- und Verzeichnis-Sync.
* **Plandatei:** JSON `{"contract":"NHYDB027_EXTERNAL_LAYOUT_V1","db_id","entries":[1..2],"mac"}`
  mit `mac = HMAC-SHA256(SHA256("NHYDB027-LAYOUT-MAC:" ‖ key), json(Umschlag ohne mac))`.
* **Eintrag:** `{"generation","sha256","nonce","ciphertext"}`; AES-256-GCM
  mit 12-Byte-Zufallsnonce, AAD `"NHYDB027:{db_id}:{generation}:{sha256}"`,
  Base64-kodiert.
* **Klartext:** Schema-Snapshot
  `{"contract":"NHYDB027_SCHEMA_V1","realms","shapes","extensions","layouts","indexes","math_layouts"}`
  (Listen nach ID sortiert).
* JSON überall mit sortierten Schlüsseln, Trennzeichen `,`/`:` und UTF-8 ohne
  ASCII-Escape; MAC und Hash hängen an dieser bytegenauen Serialisierung.

### 8.4 Öffnen (fail closed)

```mermaid
sequenceDiagram
    participant A as Aufrufer
    participant D as Db::open_locked
    participant F as X.nhydb
    participant K as layout.key
    participant P as layout.enc
    A->>D: NHyDB(path) / Db::open_with
    D->>D: Konfiguration auflösen, Sperre <db>.lock erwerben
    D->>F: beide Superblöcke lesen, höchste gültige Generation wählen
    D->>D: last_valid_end ∈ [8192, Dateigröße] prüfen
    D->>F: EIN Scan bis last_valid_end (Header-CRC aller Blöcke)
    Note over D: Datenblöcke registrieren,<br/>SYSTEM-Kette prüfen,<br/>Layout-Metablöcke ablehnen
    D->>D: Dateiname == R0,v2 ?
    D->>K: Schlüssel laden (Contract, db_id, db_name, Länge 32)
    D->>P: Plan lesen, HMAC prüfen
    D->>D: genau ein Eintrag mit (Generation, Hash) aus R0
    D->>P: entschlüsseln (AES-GCM), SHA-256 und Contract prüfen
    D->>F: erst jetzt: unkommittierten Tail abschneiden
    D->>D: Schema importieren, MATH-Zustände validieren
    D-->>A: geöffnete Datenbank
```

Jeder Fehler vor dem Abschneiden des Tails lässt die Datei unverändert.
Die Engine prüft R0 und registriert Datenblöcke in **einem** Scan über die
Blockheader.

### 8.5 Commit mit Schemaänderung (Zwei-Generationen-Protokoll)

```mermaid
sequenceDiagram
    participant D as Db::commit
    participant P as layout.enc
    participant F as X.nhydb
    D->>D: Snapshot exportieren, Generation g+1 versiegeln
    D->>P: Plan atomar ersetzen durch [aktiv(g), neu(g+1)]<br/>(temp. Datei, Sync, atomarer Austausch, Verzeichnis-Sync)
    D->>F: SYSTEM-Block (g+1, neuer Hash) anhängen
    D->>F: Puffer schreiben, Sync
    D->>F: Superblock in den anderen Slot, Sync
    Note over D: Fehler nach dem Planersatz ⇒<br/>Datenbank „poisoned“: weitere Schreibzugriffe gesperrt
```

Crash-Analyse:

| Absturzzeitpunkt | Zustand beim nächsten Öffnen |
|---|---|
| vor dem Planersatz | alte Generation g im alten Plan; R0 zeigt auf g |
| nach Planersatz, vor dem Superblock | R0 (committet) zeigt auf g; g ist im neuen Plan enthalten ⇒ öffnet; Daten im Tail werden verworfen |
| nach dem Superblock | R0 zeigt auf g+1; g+1 im Plan ⇒ öffnet |

Der Plan hält nur **zwei** Generationen. Fiele der Superblock auf einen
Stand vor g zurück (beide Superblöcke einer neueren Generation defekt), wäre
die passende Generation nicht mehr im Plan; das Öffnen schlägt dann fail
closed fehl.

---

## 9. Schreib- und Lesepfade, Commit und Persistenz

### 9.1 Append-only und Commit-Grenze

Alle Daten werden **angehängt**; bestehende Blöcke werden nie überschrieben.
Eine Aktualisierung ist ein neuer Block, der den älteren Zustand überlagert
(höchste Block-ID gewinnt). Gültig ist nur der Bereich bis zur Commit-Grenze
`last_valid_end` des aktiven Superblocks.

`Storage` (`storage.rs`) hält einen Append-Puffer von bis zu 8 MiB. Lesezugriffe
sehen auch noch nicht geschriebene Bytes. Ein fehlgeschlagener Append wird
vollständig zurückgenommen (`rollback_to`), damit kein halber Block hinter
eine spätere Commit-Grenze gelangt.

### 9.2 Commit ohne Schemaänderung

```text
1. Append-Puffer schreiben, Datei synchronisieren (Durability-Modus)
2. Superblock mit generation+1 in den anderen Slot schreiben, synchronisieren
```

Nach Rückkehr von `commit()` (bzw. einer Operation mit `commit=True`) ist
der Zustand gemäß dem gewählten Durability-Modus dauerhaft. Ohne Commit
geschriebene Daten liegen im Tail und gehen bei einem Absturz verloren.

### 9.3 Schreibpfade im Überblick

| Operation | Ergebnis im Container | Hinweise |
|---|---|---|
| `put_dense_batch(realm, u_start, rows, …)` | ein `DATA_DENSE`-Block (+ optional ein `SPARSE_CELLS`-Block für Extensions) | Extensions werden **vor** dem ersten Append validiert und kodiert |
| `put(realm, u, values, …)` | wie `put_dense_batch` mit einer Zeile | |
| `put_points(realm, cells, …)` | ein `SPARSE_CELLS`-Block je `u`-Fenster | alle Zellen werden vor dem ersten Append kodiert |
| `put_extensions(…)`, `put_extension_list(…)`, `put_extension_range(…)` | Sparse-Zellen | `merge=False` löscht nicht genannte, lebende Extensions per Tombstone |
| `delete_point(realm, u, v, …)` | Sparse-Tombstone | |
| `delete(realm, u, …)` | `DELETE`-Block | verdeckt den Dense-Record an `u` |
| `put_object(s_batch)` | Zellen gemäß Layout | |
| `put_math_batch(…)` | Sparse-Zellen in exklusiven MATH-Realms | vollständiger Rollback im Fehlerfall (Abschnitt 13) |
| `set_max_db_size(…)` | `DB_CONFIG`-Block | |
| Schemaoperationen | keine Datei-Bytes; `layout_dirty` | neue Plangeneration beim Commit |

```mermaid
flowchart LR
    IN["API-Aufruf<br/>(Python oder Rust)"] --> VAL["Validieren<br/>(Realm, Shape, Typen, Koordinaten,<br/>MATH-Besitz)"]
    VAL --> ENC["vollständig kodieren<br/>(Dense-Rows / Sparse-Stream)"]
    ENC --> LIM{"align8(Blockende)<br/>≤ max_db_size_bytes?"}
    LIM -- nein --> ERR["DatabaseSizeLimitError"]
    LIM -- ja --> APP["Block anhängen<br/>(Header + Payload + Padding)"]
    APP --> REG["Segment registrieren<br/>(Intervallindex, Caches)"]
    REG --> C{"commit=True?"}
    C -- ja --> COMMIT["commit()"]
    C -- nein --> TAIL["bleibt im Tail"]
```

**Grenze der Atomarität:** Eine Operation, die mehrere Blöcke schreibt (z. B.
`put_points` über mehrere `u`-Fenster), ist nicht als Ganzes atomar. Schlägt
ein späterer Append fehl (z. B. Größengrenze), bleiben frühere Blöcke
derselben Operation unkommittiert im Tail, bis ein Commit sie dauerhaft
macht oder die Datenbank ohne Commit geschlossen wird. Ein vollständiger
Rollback ist nur für `put_math_batch` implementiert.

### 9.4 Lesepfade

* **Dense:** `resolve_segment(realm, u, lane)` sucht im Intervallindex
  (`interval.rs`) alle Segmente, die `u` enthalten, und wählt das mit der
  höchsten Block-ID, sofern kein jüngerer `DELETE`-Block existiert. Das
  Recordbyte-Fenster ergibt sich aus der Offsettabelle.
* **Sparse:** Für COLUMN/PACKED-Zugriffe baut die Engine je `(realm, lane, u)`
  einmalig einen **v-Index** (`vindex.rs`): v-Bereich jedes Sparse-Records im
  Intervallindex und Sprungmarken alle 128 Zellen. Ein Punktzugriff prüft nur
  passende Records vom neuesten zum ältesten Block und dekodiert nur die
  Trefferzelle. Budget: 4 194 304 Sprungmarken, 4 096 v-Indizes, bis zu 64
  gemeinsam indexierte Geschwister-u-Gruppen eines Blocks.
* **Overlays:** `get_internal` kombiniert Dense-Record und neueste
  Sparse-Zustände derselben u-Gruppe.
* **Block-Cache:** CRC-geprüfte Payloads bis 8 MiB je Block, Gesamtbudget
  64 MiB (bei Überschreitung wird der Cache geleert).

### 9.5 Iteratoren

`range`, `select`, `select_us`, `find`, `iter_materialized_sparse_points`,
`iter_materialized_cells` und `iter_materialized_record_ns` liefern in
Python **lazy Iteratoren** (`ResultIterator`): Ergebnisse werden blockweise
nachgeladen, Fehler treten erst beim ersten `next()` auf, der
Speicherbedarf ist unabhängig von der Ergebnisgröße.

### 9.6 Durability-Modi

| Modus | macOS | Linux | Windows |
|---|---|---|---|
| `fsync` (Standard) | `fsync(2)` | `fsync(2)` | `FlushFileBuffers` |
| `full` | `fcntl(F_FULLFSYNC)` | `fsync(2)` | `FlushFileBuffers` |

* `fsync` ist der gewöhnliche Datei-Sync des Betriebssystems und wird auf Unix über
  `libc::fsync` aufgerufen – bewusst **nicht** über
  `std::fs::File::sync_all`, das auf Apple-Plattformen intern `F_FULLFSYNC`
  verwendet und damit bereits dem Modus `full` entspräche (im Kernbenchmark,
  Abschnitt 18.5, liegt der Schreibdurchsatz mit `full` je nach Szenario um
  rund 30–60 % niedriger).
* Auf macOS garantiert `fsync(2)` nur die Übergabe an das Laufwerk, nicht das
  Leeren seines Schreibcaches; `full` garantiert zusätzlich das Leeren.
* Unter Linux und Windows führt `full` denselben Aufruf aus wie `fsync`
  (Linux: `fsync(2)` leert laut Kernel-Vertrag auch den Laufwerkscache;
  Windows: `FlushFileBuffers`).
* Wird ein Modus nicht unterstützt oder schlägt `F_FULLFSYNC` auf macOS fehl
  (z. B. auf einem Dateisystem ohne Unterstützung), schlägt die Operation mit
  einem Fehler fehl; es gibt **keinen stillen Rückfall** auf `fsync(2)`.
* Alle Datei-Syncs (Datenbank, Layoutplan) folgen dem gewählten Modus, unter
  Unix auch die Verzeichnis-Syncs. Unter Windows ist der Verzeichnis-Sync
  (`sync_dir`) in beiden Modi wirkungslos (Verzeichnis-Metadaten schützt das
  NTFS-Journal); der atomare Austausch (`atomic_replace`) verwendet dort
  `MoveFileExW` mit `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`,
  unter Unix `rename` mit anschließendem Verzeichnis-Sync.

### 9.7 Lebenszyklus

| Methode | Wirkung |
|---|---|
| `NHyDB.create(path, overwrite=False, …)` | legt Triade an (Schlüssel, Plan mit Generation 0, DB mit SYSTEM-Block), setzt die Größengrenze (`DB_CONFIG`) und committet |
| `NHyDB(path, …)` / `nhydb.open` | öffnet fail closed (Abschnitt 8.4) |
| `commit()` | Commit-Protokoll; Rückgabe: Commit-Grenze |
| `sync(hard=False)` | Puffer schreiben, optional Datei-Sync – **kein** Superblock-Commit |
| `close(commit=True)` | optional Commit, danach Dateihandle und Sperre freigeben (sofort, nicht erst bei der Objekt-Freigabe); anschließend löst jeder Dateizugriff `NHyDBError` ("database is closed") aus; wiederholte Aufrufe sind wirkungslos; schlägt der Commit fehl, bleibt die Datenbank geöffnet (erneuter Versuch oder `close(commit=False)`); `close(commit=False)` gibt Handle und Sperre auch dann frei, wenn das Schreiben des Puffers fehlschlägt |
| `with nhydb.create(...) as db:` | `__exit__` ruft `close(commit=True)` auf – **auch wenn im Block eine Ausnahme auftrat** |
| Objekt-Freigabe ohne `close` | Puffer wird geschrieben, aber nicht committet |

---

## 10. Layouts, Routing und Layout-Evolution

### 10.1 Die vier Geometrien

| Layout | Record-ID | Feld | Anker (Record-Adresse) | Parameter |
|---|---|---|---|---|
| CLASSIC | `u` | `v` (Shape-Feld oder Extension) | `v = 1` | `shape_id` (Pflicht) |
| COLUMN | `v` | `u` (je Feld ein `u`) | `u = anchor_u` (Standard: kleinstes Feld-u) | `fields: [{u, name, type}]` |
| PACKED | `⌊(v−1)/span⌋ + 1` | Offset `(v−1) mod span` | `v = (record−1)·span + 1` | `u`, `span` (Standard 10^8), `fields: [{offset, name, type}]` |
| CUSTOM | durch Plugin | durch Plugin | durch Plugin | `plugin` (persistierte ID), `params`, `spacemap` |

Für CLASSIC, COLUMN und PACKED ist die SpaceMap aus der Mappinglogik
abgeleitet und **read-only**; eine abweichende SpaceMap wird abgelehnt.

### 10.2 Mixed-Realm-Routing

Mehrere Layouts dürfen sich einen Realm teilen. Für `(realm, u)` gewinnt
unter allen Layouts, deren u-Bereich `u` enthält:

1. höhere `priority`,
2. bei Gleichstand der **engere** u-Bereich (`u_end − u_start`; unbegrenzt
   zählt als `10^200`),
3. bei Gleichstand die höhere `layout_id`.

(`LayoutDef::sort_cmp`, `Db::layout_at`, `Db::layout_candidates`.)
`route_address` und `layout_diagnostics` machen das Routing sichtbar
(Gewinner, Kandidaten, Überlappungen, Lücken).

### 10.3 Schutz materialisierter Daten

Beim Anlegen, Ersetzen (`replace=True`) oder Evolvieren eines Layouts
berechnet die Engine die Intervalle, deren Routing sich ändern würde
(`routing_change_intervals`), und prüft, ob dort bereits Dense-Records oder
Sparse-Zellen liegen (`occupied_routing_impact`). Würde materialisierter Raum
den Layout-Eigentümer wechseln, wird die Änderung mit `SchemaError`
abgelehnt. Leerer Adressraum darf umgeroutet werden.

| Methode | Zweck |
|---|---|
| `plan_layout_evolution(layout_id, …)` | Trockenlauf: zeigt Routing- und Eigentumsauswirkungen |
| `evolve_layout(layout_id, …, commit=False)` | wendet eine sichere Evolution an (`name`, `description`, `u_start`, `u_end`, `priority`, `add_fields`, `spacemap_update` – Letzteres nur CUSTOM) |

### 10.4 CUSTOM-Layouts und Plugins

Ein CUSTOM-Layout persistiert nur seine Plugin-ID. Die Adresslogik liefert
ein zur Laufzeit registriertes Objekt:

```python
db.register_layout_plugin(name, obj)
```

Das Objekt kann die optionalen Methoden `logical_address`, `describe`,
`type_of`, `spacemap` und `occupied_addresses(db, layout, object_id, lane)`
besitzen; fehlende Methoden werden per `hasattr`
erkannt; die Engine verwendet dann ihr eingebautes Standardverhalten. Im Kern ist die
Schnittstelle das Trait `LayoutPlugin` (`plugin.rs`); die PyO3-Schicht
stellt eine Brücke für Python-Objekte bereit.

* Ohne registriertes Plugin schlagen Zugriffe, die die Adressfunktion
  benötigen, mit `SchemaError("CUSTOM layout requires registered plugin '…'")`
  fehl; die Logik wird nicht simuliert.
* **Einschränkung:** Rückrufe eines Plugins in dieselbe Datenbank während
  eines laufenden Schreibvorgangs (`put_object`, `put_objects_batch`,
  `delete_record` …) werden nicht unterstützt und mit einem PyO3-Borrow-Fehler
  (`RuntimeError`) abgewiesen. Lesende Rückrufe außerhalb von
  Schreibvorgängen funktionieren.

### 10.5 Beispiel COLUMN und PACKED

```python
db.create_realm(2, "col")
db.create_column_layout(10, 2, "colz",
    fields=[{"u": 1, "name": "a", "type": "varuint"},
            {"u": 2, "name": "b", "type": "utf8"}])
db.put_object(10, 42, {"a": 7, "b": "x"}, commit=True)
db.get_object(10, 42)
# {'_layout': 10, '_layout_name': 'colz', '_kind': 'COLUMN', '_object': 42,
#  '_lane': 0, '_lane_name': 'NL', 'a': 7, 'b': 'x'}

db.create_realm(3, "pk")
db.create_packed_layout(11, 3, "packed", u=1, span=8,
    fields=[{"offset": 0, "name": "a", "type": "varuint"},
            {"offset": 1, "name": "b", "type": "utf8"}])
db.put_object(11, 3, {"a": 1, "b": "p"}, commit=True)
db.record_anchor(11, 3)["N"]   # Anker v = (3-1)*8+1 = 17 in Realm 3
```

---

## 11. Geometrisches GET: Zelle, Record, Auto

Eine Zahl `N` bezeichnet zunächst nur **eine** mathematische Zelle. Wie sie
gelesen wird, bestimmt der Modus:

| Modus | Verhalten |
|---|---|
| `cell` | gibt genau die adressierte Zelle zurück (auch wenn sie ein Record-Anker ist) |
| `record` | interpretiert die Adresse über das geroutete Layout, bestimmt den logischen Record und gibt dessen **tatsächlich belegte** Zellen zurück; ohne auflösbaren Record `SchemaError` |
| `auto` | Anker eines Records → `record`, sonst `cell` (`auto_policy = "anchor_record_else_cell"`) |

Weitere Regeln:

* Der kanonische `record_N` wird aus der **geometrischen Ankeradresse**
  berechnet, auch wenn der Anker physisch leer ist.
* Ein PACKED-Record umfasst auch belegte, nicht als Feld deklarierte Offsets.
* Für nicht definierte Realms liefert GET
  `{"result": "unresolved", "reason": "realm_not_defined", …}`.
* `get_n(value, mode=…)` und `get_address(realm, u, v, lane=…, mode=…)`
  verwenden denselben aufgelösten Zugriff.
* `describe_n(N)` liefert Adresse, Layout, Interpretation und Belegung.
* `occupied_n(layout_id, object_id)` und `occupied_from_n(N, output="N"|"ADDRESS"|"BOTH")`
  listen die belegten Zellen eines logischen Objekts.

Beispiel (CLASSIC-Realm 1, Record `u = 5`, Feld `v = 2`):

```python
n = nhydb.dynadron_n(1, 5, 2, 0)          # 44
db.get_n(n, mode="cell")
# {'result': 'cell', 'N': 'N44', 'realm': 1, 'u': 5, 'v': 2, 'lane': 0, 'lane_name': 'NL',
#  'layout_id': 1, 'layout_name': 'records', 'layout_kind': 'CLASSIC', 'object': 5,
#  'record_N': 'N31', 'field': 'status', 'anchor': False, 'present': True, 'value': 'idle',
#  'requested_mode': 'cell', 'effective_mode': 'cell'}
db.get_n(nhydb.dynadron_n(1, 5, 1, 0), mode="auto")["effective_mode"]   # 'record'
```

---

## 12. Sekundärindizes und Volltextsuche

### 12.1 Indexfamilien

| Familie | Optionen (normalisiert, persistiert) | Schlüssel je Wert | Anlegen |
|---|---|---|---|
| VALUE | `{}` | `(value,)` (auch `None`) | `create_index(…)` |
| TOKEN | `tokenizer="simple"`, `casefold=True` | eindeutige `\w+`-Tokens | `create_token_index(…)` oder `create_index(…, kind="TOKEN")` |
| TRIGRAM | `ngram ∈ [2, 16]` (Standard 3), `casefold=False` | eindeutige Code-Point-N-Gramme | `create_trigram_index(…)` |
| FULLTEXT | `tokenizer="simple"`, `casefold=True`, `positions=True` | Terme mit Positionen, Dokumentlängen | `create_fulltext_index(…)` |

* Nicht-VALUE-Indizes sind nur auf `utf8`-Quellfeldern (BASE oder Extension)
  zulässig.
* Textnormalisierung: `None → ""`, sonst Python-`str(value)`, optional
  `casefold()`. Zeichenklassen (`\w`) und `casefold` sind aus CPython 3.14.5
  (Unicode 16.0.0) generiert und über alle 0x110000 Code Points gegen
  CPython getestet.
* N-Gramme und `contains` arbeiten auf Code Points, nicht auf Bytes.

### 12.2 Aufbau und Frische

`build_indexes(index_ids=None, commit=False)` liest BASE-Felder über einen
selektiven Decoder und Extensions direkt aus Sparse-Blöcken (neuester Block
gewinnt, Tombstones und Deletes werden beachtet) und schreibt je Index und
Lane einen SECONDARY-Block. Ein Index ist **frisch**, wenn sein
SECONDARY-Block jünger ist als der jüngste Datenblock der Lane
(`index_is_fresh`). Die direkten Index-APIs (`find_u`/`find`,
`index_candidates`) lösen bei einem veralteten Index `StaleIndexError` aus.
Der Planer der QueryEngine verwendet nur frische Indizes und weicht sonst auf
einen Scan aus; die BM25-Suche (`search`) verlangt einen frischen
FULLTEXT-Index. `verify` meldet veraltete Indizes.

Der Index eines Realms, der nur Sparse-Daten enthält, wird geschrieben und
gilt als frisch; Sparse-Zustände von BASE-Feldern (inkl. Tombstones und
reiner Sparse-Records) werden beim Indexaufbau berücksichtigt.

### 12.3 Kandidaten-API am Kern

| Methode | Zweck |
|---|---|
| `find_u(realm, filters, …)` / `find(…)` | VALUE-Filter (Gleichheit, Bereich) → sortierte `u` bzw. Records (Iterator) |
| `posting_count(index_id, cond)` | Trefferanzahl aus dem Verzeichnis ohne Dekodierung |
| `index_candidate_estimate(index_id, op, value, value2, …)` | Schätzung für den Planer |
| `index_candidates(index_id, op, value, value2, …)` | sortierte Kandidaten-`u` (VALUE `eq/between`, TOKEN `token/tokens_all/tokens_any`, TRIGRAM `contains/icontains`, FULLTEXT `fulltext/phrase`) |
| `fulltext_term_posting`, `fulltext_corpus`, `fulltext_payloads` | Zugriff auf FULLTEXT-Payloads |

### 12.4 BM25

```text
idf   = ln(1 + (N − df + 0.5) / (df + 0.5))
avgdl = total_tokens / N          (0 ⇒ 1.0)
score += idf · tf · (k1 + 1) / (tf + k1 · (1 − b + b · dl / avgdl))
k1 = 1.2, b = 0.75 (Standard)
```

Die Summation erfolgt in Anfrageterm-Reihenfolge in `f64`; sortiert wird nach
`(-score, u)`, also bei Gleichstand nach `u`.

---

## 13. MATH_MAP: mathematische Belegung

### 13.1 Prinzip

Ein MATH-Layout bildet **Eingabetupel** über arithmetische Ausdrücke auf
Zellen ab:

```text
inputs (x1, …, xk)  ──MathMap──►  (u, V_global, side)
V_global  ──split──►  (logischer Realm, v_local)     mit T = 10^100
logischer Realm  ──RealmMap──►  physischer Realm
gespeicherte Zelle:  (physischer Realm, u, v_local, lane = side)
```

| Aspekt | Art |
|---|---|
| Abbildung `inputs → (u, V, side)` über benutzerdefinierte Ausdrücke | Anwendungsmodell |
| Zerlegung `V → (R_logisch, v)` mit `T = 10^100` | mathematisches Prinzip (Abschnitte 2.3 und 2.11) |
| Zuordnung logischer zu physischen Realms über eine append-only RealmMap | **Implementierungsentscheidung**: MATH-Layouts belegen nur freie Realm-IDs und besitzen diese exklusiv |
| `N` einer MATH-Zelle | wird aus der **physischen** Zelladresse berechnet |

### 13.2 Ausdruckssprache

Die Sprache bildet die Python-Ausdrucksgrammatik nach (`mathexpr.rs`); `^`
wird als `**` gelesen.

| Erlaubt | Beispiele |
|---|---|
| Ganzzahl- und Bool-Konstanten, Literale `0x…`, `0o…`, `0b…`, `1_000` | `10**100` |
| Parameternamen (Identifier, nicht mit `_` beginnend, keine Funktionsnamen) | `x`, `y` |
| Operatoren `+ - * // % **`, unär `+ - not` | `(x - 1) // 2` |
| `and`/`or` (Kurzschluss), Vergleichsketten `== != < <= > >=` | `0 < x < 10` |
| Funktionen `abs`, `gcd`, `lcm`, `isqrt`, `min`, `max` (nur Positionsargumente) | `gcd(x, 6)` |

Nicht erlaubt (Auswahl): Attribute, Indizes, Division `/`, Bitoperatoren,
bedingte Ausdrücke (`a if c else b` → `MathSyntaxError("syntax element IfExp is not allowed")`).
Semantik nach Python: beliebig große Ganzzahlen, Floor-Division und Modulo
mit dem Vorzeichen des Divisors, `**` mit negativem Exponenten ⇒
`MathDomainError`, `bool` in Arithmetik verboten.

**Ressourcengrenze:** Kein Zwischenergebnis
darf mehr als `math_max_intermediate_bits` Bit haben (Standard
2^20 = 1 048 576). `*`, `**` und `lcm` werden **vor** der Berechnung über die
Bitlängen abgeschätzt; Überschreitung ⇒ `MathDomainError`.

```python
from nhydb.math import MathExpression
MathExpression.compile("2**3 + gcd(x, 6)", ("x",)).evaluate(x=4)   # 10
```

### 13.3 MathMap und Spezifikation

```python
from nhydb.math import MathMap, MathMapLayoutSpec
m = MathMap.compile("grid", ("x", "y"), u="x+1", V="y", side="x % 2")
spec = MathMapLayoutSpec("grid", m, 4096)      # realm_auto_limit = 4096
```

* `side` ist fest (`"NL"`, `"NR"`, `0`, `1`, `bool`) oder ein Ausdruck mit
  Ergebnis 0/1/bool.
* `target(*inputs)` verlangt `u > 0`, `V > 0`.
* Der **Fingerprint** ist SHA-256 über das kanonische JSON mit
  `ensure_ascii=True`. `from_dict` prüft ihn.
* `dry_run(mapping, rows, …)` liefert eine Diagnose (gültige/ungültige Zeilen,
  Kollisionen, benötigte Realms, Budget) ohne Datenbankzugriff.

### 13.4 Realmplaner (`mathlayout.rs`)

* Benötigte logische Realms werden zu zusammenhängenden Extents gruppiert;
  jedes Extent wird in den **ersten freien physischen Lauf** gelegt, der es
  ganz aufnimmt (beginnend bei Realm 1; jeder Nicht-MATH-Realm gilt als
  belegt).
* Budget: `bestehende + neue ≤ realm_auto_limit` (Standard 4096), sonst
  `RealmBudgetExceeded`.
* Pläne werden gegen einen Snapshot (Revision = `next_block_id`) validiert
  (`StalePlanError`, `AllocationConflictError`).
* Die RealmMap ist append-only und bijektiv.

### 13.5 Core-API

| Methode | Beschreibung |
|---|---|
| `create_math_layout(spec, *, layout_id=None, payload_type="bytes", commit=False)` | legt ein MATH-Layout an (freie ID = kleinste unbenutzte) |
| `create_math_map_layout(name, parameters, *, u, V, side=None, …)` | Kurzform mit Ausdrücken |
| `replace_math_spec(layout_id, spec, …)` | nur vor dem ersten Schreiben, sonst `MappingFrozenError` |
| `put_math_batch(layout_id, rows, *, overwrite=False, commit=False)` | Zeilen `(inputs, payload)` |
| `put_math_numpy(layout_id, inputs, payloads, …)` | NumPy-Variante (Abschnitt 16.6) |
| `put_math(layout_id, inputs, payload, …)` | eine Zeile; Rückgabe der physischen Adresse |
| `get_math(layout_id, inputs)` | Wert oder `None` (allokiert nie) |
| `get_math_cell(layout_id, inputs, *, dyn_address=False)` | Zellen-Dict mit `_math_layout_id`, `_logical_realm`, `_V_global`, `_math_inputs` |
| `describe_math(layout_id)` | Zustand, Mapping, RealmMap, Anzahl Zellen |

### 13.6 Ablauf von `put_math_batch`

```mermaid
flowchart TB
    A["Zeilen (inputs, payload)"] --> B["Ziele berechnen<br/>(MathMap.target, Bitgrenze)"]
    B --> C{"logisches Duplikat<br/>und overwrite=False?"}
    C -- ja --> X1["MathCellConflictError"]
    C -- nein --> D["benötigte logische Realms"]
    D --> E["Snapshot → plan_math_realms<br/>→ require_allowed → validate_plan"]
    E --> F{"Zielzelle bereits belegt<br/>und overwrite=False?"}
    F -- ja --> X2["MathCellConflictError"]
    F -- nein --> G["Rücksetzpunkt merken"]
    G --> H["physische Realms anlegen,<br/>Zustand einfrieren (frozen)"]
    H --> I["Sparse-Zellen je (Realm, Seite),<br/>u-Fenster 10^8"]
    I --> J["Zustand validieren, optional Commit"]
    I -. Fehler .-> RB["Rollback: Datei kürzen,<br/>Zustand aus Datei + vorigem Schema neu aufbauen"]
    J -. Fehler .-> RB
```

### 13.7 Exklusivität

Physische Realms eines MATH-Layouts sind für Shapes, Layouts, Extensions,
Indizes und direkte `put_points` gesperrt
(`SchemaError("Realm … belongs exclusively to MATH layout …")`).
`create_realm` auf einer solchen Realm-ID ist nicht
gesperrt und ändert nur Name/Beschreibung.

### 13.8 Beispiel

```python
from nhydb.math import MathMap, MathMapLayoutSpec
m = MathMap.compile("grid", ("x", "y"), u="x+1", V="y", side="x % 2")
db.create_math_layout(MathMapLayoutSpec("grid", m, 4096), layout_id=7, payload_type="utf8")
db.put_math_batch(7, [((x, y), f"{x},{y}") for x in range(1, 5) for y in (1, 10**100 + 1)],
                  commit=True)
# {'cells': 8, 'blocks': 4, 'bytes': 508, 'new_realms': 2, 'mapped_realms': 2,
#  'realm_extents': [{'logical_start': 1, 'logical_end': 2,
#                     'physical_start': 4, 'physical_end': 5}]}
db.get_math(7, (3, 10**100 + 1))           # '3,1000…0001'
db.get_math_cell(7, (3, 10**100 + 1))      # u=4, physischer Realm 5, NR, _logical_realm=2
```

In diesem Lauf waren die Realms 1–3 bereits durch gewöhnliche Layouts belegt;
der Planer legte die logischen Realms 1 und 2 deshalb auf die physischen
Realms 4 und 5.

---

## 14. Abfragesysteme: QueryEngine und NHySQL-0.24

### 14.1 Pipeline

```mermaid
flowchart LR
    SQL["NHySQL-Text"] --> PARSE["Lexer + Parser<br/>(sql.rs)"]
    PARSE --> COMP["compile_sql<br/>(Quelle, Felder, Regeln)"]
    QB["Q-Builder (Python)"] --> AST["Ausdrucksbaum Expr"]
    COMP --> AST
    AST --> PLAN["Planer cardinality-cost-v1"]
    PLAN --> FP{"Fastpath?"}
    FP -- "POSTING_COUNT" --> OUT
    FP -- "CLASSIC_U" --> OUT
    FP -- nein --> EVAL["Auswertung über Provider:<br/>Index · Scan · Struktur · MATH · N"]
    EVAL --> CAND["Kandidaten (sortierte N / record_N)"]
    CAND --> OUT["Ausgabe: count · n · cell · record ·<br/>fields · aggregate · ranked"]
```

### 14.2 Ausdrucksbaum und Builder

Knoten: `BoolExpr(and|or|not)`, `FieldPredicate`, `StructPredicate`,
`MathPredicate`, `DirectNPredicate`, `AllPredicate`. Python-Operatoren `&`,
`|`, `~` und Vergleiche auf Referenzen erzeugen Knoten.

| Builder | Bedeutung |
|---|---|
| `Q.field(name)` | Feldreferenz mit `eq ne lt le gt ge between contains icontains token tokens_all tokens_any fulltext phrase exists` (auch über `== != < <= > >=`) |
| `Q.realm`, `Q.u`, `Q.v`, `Q.lane`, `Q.N`, `Q.record_N`, `Q.struct(name)` | strukturelle Prädikate |
| `Q.n(*values, domain=…)` | direkte N-Menge |
| `Q.math(provider, **params)` | Provider-Prädikat |
| `Q.fulltext(field, …)`, `Q.phrase(field, …)` | Volltext |
| `Q.all()` | alle Kandidaten |

### 14.3 QueryEngine

```python
from nhydb.query import Q, QueryEngine
q = QueryEngine(db)
q.execute(expr, *, realm=None, lane=None, return_mode=None, select=None, aggregate=None,
          group_by=None, order_by=None, limit=None, offset=None, explain=None, k1=1.2, b=0.75)
q.explain(expr, *, realm=None, lane=None, return_mode=None)
q.search(expr, *, realm=None, lane=None, select=None, limit=20, offset=None, explain=None, k1=1.2, b=0.75)
q.execute_spec(spec)                         # JSON-Spezifikation
q.register_math_provider(name, function, *, domain=None, description=None, estimator=None)
q.execute_numpy(expr, *, realm=None, lane=None, select, order_by=None, limit=None, offset=None)
q.compile_sql(text, parameters=None, *, lane=None)
q.execute_sql(text, parameters=None, *, lane=None)
q.execute_sql_numpy(text, parameters=None, *, lane=None)
```

**Rückgabemodi:** `record`, `cell`, `n`, `count`, `fields`, `aggregate`,
`ranked` (= `search`).

**Planer `cardinality-cost-v1`:** Indexzugriff kostet `8 + rows` (zuzüglich
`rows` bei TRIGRAM, `0,5·rows` bei FULLTEXT), ein Scan die
Universumsschätzung. `AND` sortiert Kinder nach (Zeilen, Kosten, Position)
und bricht bei leerer Menge früh ab; `OR` summiert; `NOT` = Universum minus
Kind. Strukturprädikate nutzen `DIRECT_N`, `STRUCT_DIRECT`, `STRUCT_RANGE`.
Kosten werden wie Python-`round(cost, 3)` gerundet.

**Fastpaths:**

* `POSTING_COUNT`: `count` über genau ein Feldprädikat mit exakter Schätzung.
* `CLASSIC_U`: Record-Domäne in einem Realm mit genau einem CLASSIC-Layout,
  nur indizierte Feldblätter in `AND`; Auswertung im u-Raum.

**Provider:** eingebaut `formula` (Ausdrücke über `k`), `n_sequence`
(arithmetische N-Folge, GZ/N0 übersprungen), `math_layout_inputs`
(Eingabetupel eines MATH-Layouts); zusätzlich Laufzeit-Provider als
Python-Callables, die die Engine erhalten und **lesend** auf `engine.db`
zugreifen dürfen.

**Semantik:** Vergleiche, Gleichheit und Gruppenschlüssel nach Python
(`1 == 1.0 == True`; inkompatible Ordnungsvergleiche ergeben `False`);
Gruppen in Einfügereihenfolge; `sum` exakt für Ganzzahlen; `avg` als korrekt
gerundete Division; `ORDER BY` stabil mit fester `None`-Behandlung,
inkompatible Typen ⇒ `QueryPlanError`.

**Leistungsoptimierungen ohne Semantikänderung:** Zeilen ohne `ORDER BY`
werden erst im LIMIT/OFFSET-Fenster erzeugt; Materialisierungsscans halten
keine Menge aller Records; `STRUCT_RANGE` über Bereiche > 4096 besucht nur
materialisierte `u`.

### 14.4 NHySQL-0.24 (lesendes SELECT)

Grammatik (vereinfacht):

```text
[EXPLAIN [QUERY PLAN]]
SELECT select_list
FROM [realm.]layout [[AS] alias]
[WHERE or_expr]
[GROUP BY ident, …]
[ORDER BY path [ASC|DESC], …]
[LIMIT literal] [OFFSET literal] [;]
```

| Element | Unterstützung |
|---|---|
| SELECT-Liste | `*`, `q.*`, Feld `[AS] alias`, `relevance`, `COUNT/SUM/MIN/MAX/AVG(*\|feld)` (Standardalias `<op>_<feld\|all>`) |
| Quelle | nur **CLASSIC**-Layouts; Realm als `realm_N`, `realmN`, `rN`, `N` oder eindeutiger Name; Layout als Zahl, `layout_N`, `layoutN` oder eindeutiger Name |
| WHERE | `NOT` > `AND` > `OR`, Klammern; `IS [NOT] NULL`, `BETWEEN … AND …`, Vergleiche mit Literal (`= NULL` verboten), Infix `CONTAINS`, `ICONTAINS`, `TOKEN`, `FULLTEXT`, `PHRASE`; Funktionen `TOKEN`, `TOKENS_ALL`, `TOKENS_ANY`, `CONTAINS`, `ICONTAINS`, `FULLTEXT`, `FULLTEXT_ANY`, `PHRASE`, `EXISTS` |
| Strukturreferenzen | `@u`, `STRUCT.u` (`realm`, `u`, `v`, `lane`, `n`, `record_n`, `layout_id`, `layout_kind`, `layout_name`, `object`) |
| Parameter | `?` (positionsnummeriert) und `:name`; werden beim Parsen eingesetzt |
| Lexik | Kommentare `--` und `/* */`, Strings `'…'` mit `''`, Identifier `"…"`/`` `…` ``, Unicode-Ziffern nach CPython-Regeln |
| Nicht implementiert | `DISTINCT`, `HAVING` (Fehler „… is not implemented in NHySQL“), alle schreibenden Anweisungen |

Regeln: Feld- und Aliasauflösung case-insensitiv (casefold); nicht
aggregierte Felder müssen gruppiert sein; `GROUP BY` ohne Aggregat ist ein
Fehler; `relevance` nur ohne Aggregat und nur mit `ORDER BY relevance DESC`
(→ BM25-Suche, Standard-`LIMIT 20`); `COUNT(*)` allein nutzt den Count-Pfad.
Fehlertexte enthalten Zeile, Spalte und `near`.

Ergebnis: `{query_version, language: "NHySQL-0.24", statement, source,
columns, rows, returned, matched, optimizer, …}`; `EXPLAIN` liefert nur den
Plan.

```python
db.sql("SELECT status, COUNT(*) AS n FROM records GROUP BY status ORDER BY n DESC")["rows"]
# [{'status': 'idle', 'n': 667}, {'status': 'active', 'n': 333}]
db.sql("SELECT id, relevance FROM records WHERE FULLTEXT(body, :q) "
       "ORDER BY relevance DESC LIMIT 5", {"q": "quick fox"})
db.sql_numpy("SELECT id, temp FROM records WHERE id BETWEEN ? AND ?", [3, 7])
# {'id': array([3, 4, 5, 6, 7]), 'temp': array([23., 24., 25., 26., 20.])}
```

---

## 15. Integrität, Recovery und Sicherheitsmechanismen

### 15.1 Prüfsummen und Strukturprüfungen

| Prüfung | Zeitpunkt |
|---|---|
| Superblock-CRC32, Magic, Version | jedes Öffnen |
| Header-CRC aller Blöcke im committeten Bereich | jedes Öffnen (ein Scan) |
| Payload-CRC von SYSTEM, DB_CONFIG, DELETE, CHECKPOINT | beim Öffnen |
| Payload-CRC von Daten- und Indexblöcken | beim ersten Laden in den Block-Cache und in `verify` |
| Präfixe, Offsettabellen, Sparse-Streams, Gruppenzahl | `verify`, mit `deep=True` vollständige Dekodierung aller Records |
| Plan: HMAC, AES-GCM-Tag, SHA-256 des Klartexts, Contracts, Generation | jedes Öffnen |
| R0-Kette: unveränderliche Zellen, Generation +1, v9 | jedes Öffnen |
| MATH-Zustände (keine Fremddefinitionen in MATH-Realms, keine Doppelbelegung) | Öffnen und `health` |

Alle Längen- und Offsetberechnungen aus Dateiinhalten sind überlaufgeprüft;
manipulierte Header ergeben `CorruptionError` statt eines Absturzes.

### 15.2 Tail-Recovery

Bytes hinter der Commit-Grenze werden beim Öffnen – erst nach erfolgreicher
Prüfung von R0, Schlüssel und Plan – abgeschnitten; ihre Anzahl steht in
`db.recovered_tail_bytes`.

### 15.3 Wartungsfunktionen

| Funktion | Beschreibung |
|---|---|
| `verify(deep=False)` | Blockstruktur bis Dateiende; Ergebnis `{ok, scanned_blocks, physical_records, error_count, errors, …}`; meldet veraltete Indizes |
| `health(deep=False)` | Checks `container`, `layouts`, `spacemap`, `layout_routing`, `math_layouts`, `definitions`, `indexes`, `checkpoint`; Gesamtstatus `healthy`/`warning`/`unhealthy` |
| `stats()` | Datei, Format, Realms, Shapes, Layouts, Extensions, Indizes (inkl. `kind`, `options`, `distinct_values`), `max_db_size_bytes`, MATH-Layouts |
| `checkpoint()` | CHECKPOINT-Block + Commit |
| `NHyDB.inspect_tail(path)` | prüft den physischen Dateischwanz ohne Öffnen (Header-, Payload-CRC) |
| `NHyDB.repair_file(path, *, apply=False, make_backup=True, rebuild_indexes=True, …)` | schneidet einen defekten Schwanz ab und setzt beide Superblöcke neu |
| `NHyDB.compact_file(path, *, output=None, make_backup=True, rebuild_indexes=True, batch_records=10000, …)` | schreibt den logischen Zustand neu |

### 15.4 Repair

`repair_file` ermittelt per `inspect_tail` das letzte CRC-gültige Blockende
und bestimmt daraus ein **sicheres** Ende: übernommen werden
nur Blöcke, deren R0-Generation im Layoutplan liegt und deren Datenblöcke auf
veröffentlichte Realms, Shapes und Indizes verweisen. Liegt das sichere Ende
vor der bisherigen Commit-Grenze, bricht `repair` mit `CorruptionError` ab
(keine automatische Reparatur committeter Inkonsistenzen). Sidecar-Pfade und
Konfiguration werden durchgereicht. Mit `make_backup=True` wird vorher
eine Kopie `<db>.bak_<zeitstempel>` angelegt.

> **Hinweis:** Weil die neue Commit-Grenze auf das sichere Ende gesetzt wird,
> können intakte, bisher **unkommittierte** Blöcke durch `repair` dauerhaft
> werden. `apply=False` (Standard) berichtet nur.

### 15.5 Kompaktierung

* **In-place** (`output=None`): neue Datei mit identischer R0-Identität (ID,
  Generation) und denselben Sidecars; nur die `.nhydb` wird atomar ersetzt;
  Backup `<db>.before_compact_<zeitstempel>` der DB-Datei bei
  `make_backup=True`.
* **Export** (`output=…`): neue UUID, eigene Sidecars neben der Zieldatei.
* Ablauf: Dense-Records mit gespeicherten BASE-Rohwerten, Sparse-Zustände
  (Tombstones nur dort, wo sie Dense-Werte verdecken), MATH-Zustände 1:1,
  Indexneuaufbau, Checkpoint, Deep-Verify der Zieldatei, erneutes Öffnen mit
  Deep-Verify, atomarer Austausch.
* In-place-Kompaktierung funktioniert auch mit MATH-Daten; Realm-Defaults
  (`default_shape_id`/`base_reserve_to`) und Tombstones auf BASE-Feldern
  bleiben erhalten.
* Die Sperre wird über Quelle, Staging-Datei und Austausch gehalten.

### 15.6 Sicherheitsmechanismen und ihre Grenzen

| Mechanismus | Schützt gegen | Schützt **nicht** gegen |
|---|---|---|
| AES-256-GCM + HMAC des Layoutplans | unbemerkte Manipulation oder Vertauschung von Schema/Plan; Lesen des Schemas ohne Schlüssel | Lesen der **Nutzdaten** in `X.nhydb` – diese sind **nicht verschlüsselt** |
| Bindung R0 ↔ Plan (Generation, SHA-256) | Kombination nicht zusammengehöriger Dateien | Verlust der Schlüsseldatei (DB dann nicht mehr zu öffnen) |
| Dateinamensbindung (R0,v2) | versehentliches Öffnen umbenannter Dateien | – |
| Schreibsperre `<db>.lock` | gleichzeitige Schreibprozesse derselben Engine | Prozesse, die die Sperre umgehen (`lock=False`) oder nicht beachten |
| Größengrenze `DB_CONFIG` | unkontrolliertes Dateiwachstum | – |
| MATH-Bitgrenze | Ressourcenerschöpfung durch Ausdrücke | – |
| CRC32 | zufällige Bitfehler, Teil-Schreibvorgänge | gezielte Manipulation der Datenblöcke (CRC ist kein kryptografischer Schutz) |

Schlüsseldatei: Unix-Rechte `0600`; unter Windows gilt die Standard-ACL des
Benutzerprofils. Geheimschlüssel gehören nie in die Konfigurationsdatei.

---

## 16. Python-Schnittstelle (PyO3) und NumPy

### 16.1 Module

| Modul | Inhalt |
|---|---|
| `nhydb` | `NHyDB`, `LayoutDef`, `IndexDef`, `QueryEngine`, `MathLayoutState`, Ausnahmen, Adressfunktionen, Konstanten, `open()`, `create()` |
| `nhydb.kernel` | zustandsloser Kernel: Adressmathematik, Codecs, Postings, Mengenkerne, Superblock/Blockheader, Plan-Krypto |
| `nhydb.index` | Indexsemantik (`simple_tokens`, `ngrams`, `index_keys`, `predicate_matches`, FULLTEXT-Codecs, `bm25_scores`, …) |
| `nhydb.math` | `MathExpression`, `MathMap`, `MathMapTarget`, `MathMapLayoutSpec`, `dry_run`, `split_global_v`, `join_global_v`, `normalize_side`, `set/get_math_max_intermediate_bits` |
| `nhydb.mathlayout` | Realmplaner (`plan_math_realms`, `validate_plan`, `resolve_math_target`, …) |
| `nhydb.query` | `Q`, Ausdrucksklassen, native `QueryEngine`, `expr_from_spec`, `parse_sql`, SQL-Dataclasses, CLI |

Modulkonstanten von `nhydb`: `REALM_SPAN` (`10**100`), `ENGINE_VERSION`
(`"0.27.0"`, die Produktversion), `FILE_FORMAT_VERSION` (`"0.16"`, interne
Kennung des Dateiformats), `TYPE_CODES`. `nhydb.query` stellt zusätzlich
`QUERY_VERSION` (`"0.27"`) und `SQL_LANGUAGE_VERSION` (`"NHySQL-0.24"`)
bereit (Abschnitt 18.3).

### 16.2 Öffnen und Anlegen

```python
db = nhydb.NHyDB.create("x.nhydb", *, overwrite=False, layout_path=None, key_path=None,
                        config=None, durability=None, lock=None, lock_timeout_ms=None)
db = nhydb.NHyDB("x.nhydb", *, strict=True, layout_path=None, key_path=None,
                 config=None, durability=None, lock=None, lock_timeout_ms=None)
```

Alle Optionen sind Schlüsselwortargumente. `nhydb.create(...)` und
`nhydb.open(...)` sind Kurzformen für `NHyDB.create(...)` bzw. `NHyDB(...)`.

Attribute: `path`, `closed`, `durability` (les- und schreibbar),
`layout_path`, `key_path`, `max_db_size_bytes`, `next_block_id`,
`last_valid_end`, `committed_end`, `checkpoint_offset`,
`recovered_tail_bytes`, `config_source`, `realms` (`{id: dict}`),
`index_defs` (`{id: IndexDef}`), `math_layouts`, `f` (lesende Dateisicht mit
`seek`/`tell`).

### 16.3 Python-API (Auswahl nach Bereichen)

| Bereich | Methoden |
|---|---|
| Schema | `create_realm`, `create_shape`, `set_realm_default_shape`, `define_extension`, `define_extensions`, `resolve_v`, `extension_catalog` |
| Layouts | `create_layout`, `create_classic_layout`, `create_column_layout`, `create_packed_layout`, `create_custom_layout`, `register_layout_plugin`, `layout_at`, `layout_candidates`, `route_address`, `layout_diagnostics`, `get_layout`, `plan_layout_evolution`, `evolve_layout`, `logical_address`, `record_anchor` |
| Schreiben | `put_dense_batch`, `put`, `put_extensions`, `put_extension_list`, `put_extension_range`, `put_points`, `delete_point`, `delete`, `put_object`, `put_objects_batch` |
| Lesen | `get`, `get_internal`, `get_cell`, `get_cell_n`, `get_object`, `get_record_view`, `get_n`, `get_n_cell`, `get_n_record`, `get_address`, `describe_n`, `occupied_n`, `occupied_from_n`, `range`, `select`, `select_us`, `iter_materialized_sparse_points`, `iter_materialized_cells`, `iter_materialized_record_ns`, `record_n_for_address`, `record_n_from_n` |
| Adressen | `realm_bounds`, `dyn_address`, `cell_address`, `resolve_n`, `set_dyn_address_output` |
| Indizes | `create_index`, `create_token_index`, `create_trigram_index`, `create_fulltext_index`, `build_indexes`, `find_u`, `find`, `posting_count`, `index_candidate_estimate`, `index_candidates`, `fulltext_term_posting`, `fulltext_corpus`, `fulltext_payloads` |
| MATH | `create_math_layout`, `create_math_map_layout`, `replace_math_spec`, `put_math_batch`, `put_math_numpy`, `put_math`, `get_math_cell`, `get_math`, `describe_math` |
| Abfragen | `query`, `explain_query`, `sql`, `sql_numpy`, `compile_sql` |
| System | `get_system_metadata`, `get_system_cell`, `set_max_db_size` |
| Wartung | `commit`, `sync`, `close`, `checkpoint`, `verify`, `health`, `stats`, `compact_file`, `inspect_tail`, `repair_file` (statisch) |

Rückgabewerte: `create_realm/shape/index` und `define_extension` liefern
Dicts (gleiche Felder wie `to_dict()`), `db.realms` liefert `{id: dict}`;
`QueryEngine.math_providers` liefert ein Dict
`{name: {name, domain, description, builtin}}`; Laufzeit-Provider erhalten
`params` als Kopie (dict/list statt tuple).

### 16.4 Einzel-API (ein Datensatz bzw. eine Zelle je Aufruf)

| Methode | Beschreibung |
|---|---|
| `put_record(layout_id, object_id, values, *, lane=None, commit=False)` | Record über ein Layout schreiben |
| `put_cell(realm_id, u, v, value, *, type=None, lane=None, commit=False)` | einzelne Zelle `(R,u,v[,lane])` |
| `get_record(layout_id, object_id, *, lane=None)` | Dict oder `None` |
| `get_value(realm_id, u, v, *, lane=None)` | Wert oder `None` |
| `delete_cell(realm_id, u, v, *, lane=None, commit=False)` | Tombstone |
| `delete_record(layout_id, object_id, *, lane=None, commit=False)` | Record logisch löschen |

`commit=True` führt dieselbe Commit-Sequenz wie `db.commit()` aus.

### 16.5 NumPy-Batch-API

Arrays werden über das Buffer-Protokoll übernommen (kontrollierte Kopie,
beliebige Endianness und Strides; kein `numpy`-Crate). Kodier-, Validier-
und Schreibschleifen laufen in Rust ohne GIL.

| Methode | Beschreibung |
|---|---|
| `put_numpy_batch(realm_id, u_start, columns, *, shape_id=None, lane=None, commit=False)` | CLASSIC/BASE-Batch ab `u_start` |
| `put_objects_numpy(layout_id, object_ids, columns, *, lane=None, commit=False, u_window=10000)` | beliebiges Layout |
| `put_cells_numpy(realm_id, u, v, values, *, type=None, lane=None, commit=False, u_window=10000)` | Sparse-Zellen |
| `read_numpy(realm_id, u_start, count, fields=None, *, lane=None)` | Spalten als Arrays + `_present` |

| Feldtyp | akzeptierte Spalten |
|---|---|
| `u8 … i64`, `varuint`, `varint` | Integer-Arrays jeder Breite (Bereich wird geprüft), Listen, BigInt-Objekte |
| `f32`, `f64` | Float- und Integer-Arrays |
| `bool` | Bool- und Integer-Arrays |
| `utf8`, `bytes`, Enums | Objekt-Arrays oder Listen (`str`, `bytes`, Enum-Labels) |

`read_numpy` liefert für `varuint` `uint64`, für Integer-Typen `int64`,
`float32/64`, `bool`, sonst Objekt-Arrays; fehlende Records haben
`_present=False` (Werte 0 / NaN / None). **Große Koordinaten:** Skalare wie
`u_start`/`realm_id` dürfen beliebig groß sein; Arrays für `u`/`v`/`object_id`
sind auf `uint64` begrenzt – für größere Werte Objekt-Arrays oder Listen von
Python-`int` verwenden.

### 16.6 NumPy für MATH und Abfragen

* `put_math_numpy(layout_id, inputs, payloads, *, overwrite=False, commit=False)`:
  `inputs` als 2D-Array (Zeilen × Parameter), 1D-Array oder Liste von
  Spalten; gleiche Semantik und bytegleiche Datei wie `put_math_batch`.
* `db.sql_numpy`, `QueryEngine.execute_numpy`, `QueryEngine.execute_sql_numpy`
  liefern `{Spalte: ndarray}`. Typregel je Spalte: nur `bool` ⇒ `bool`; nur
  Ganzzahlen im int64-Bereich ⇒ `int64`; Ganzzahlen/Floats ohne
  Genauigkeitsverlust ⇒ `float64`; sonst (None, Text, große Zahlen) ⇒ `object`
  – ohne stille NaN-Ersetzung.

### 16.7 Ausnahmehierarchie

```text
Exception
├── NHyDBError
│   ├── CorruptionError
│   ├── SchemaError
│   ├── StaleIndexError
│   ├── RealmLimitError
│   ├── AddressError
│   ├── DatabaseLockedError
│   ├── DatabaseSizeLimitError
│   └── MathLayoutError
│       ├── MathLayoutNotFound
│       ├── MappingFrozenError
│       └── MathCellConflictError
├── MathError
│   ├── MathSyntaxError
│   └── MathDomainError
├── LayoutError                  (Fehler des MATH-Realmplaners)
│   ├── LayoutStateError
│   ├── RealmBudgetExceeded
│   ├── StalePlanError
│   └── AllocationConflictError
├── QueryError
│   ├── QueryPlanError
│   ├── QueryDomainError
│   └── SQLSyntaxError
└── ValueError
    └── IndexConfigError
```

Daneben verwendet die Engine die Builtins `ValueError`, `KeyError`,
`TypeError`, `FileNotFoundError` und `FileExistsError`. `MathError`,
`LayoutError` und `QueryError` sind **keine** Unterklassen von `NHyDBError`.
Definiert ist die Hierarchie in `crates/nhydb-py/src/exceptions.rs`.

### 16.8 CLI

Das Paket installiert den Befehl `nhydb` (gleichwertig: `python -m nhydb`);
`nhydb --help` bzw. `nhydb <befehl> --help` zeigen die Optionen.

| Befehl | Zweck |
|---|---|
| `to-n`, `from-n` | Adressumrechnung ohne Datenbank |
| `layout`/`get-layout`, `layout-check`, `layout-plan`, `evolve-layout`, `route`, `logical-address` | Layouts |
| `describe`, `occupied`, `get`/`get-n`, `address`, `cell`, `cell-n`, `find` | Lesen |
| `stats`/`info`, `health`, `verify`, `rebuild-indexes`, `repair`, `compact` | Betrieb |

`get` akzeptiert `N123`, `R1,u2,v3[,NL|NR]` oder `u2,v3[,NL|NR]` mit `--realm`.

`python -m nhydb.query DB` bietet `--eq`, `--between`, `--token`,
`--contains`, `--icontains`, `--fulltext`, `--phrase`, `--u`, `--v`,
`--spec(-file)`, `--sql(-file)`, `--params`, `--return`, `--select`,
`--aggregate`, `--group-by`, `--order-by`, `--limit`, `--offset`,
`--explain`, `--json`.

```bash
nhydb from-n N589
nhydb get daten.nhydb u5,v2 --realm 1 --mode cell
python -m nhydb.query daten.nhydb --sql "SELECT COUNT(*) FROM records"
```

Die CLI kennt keine Optionen für abweichende `layout_path`/`key_path`; sie
verwendet die Standard-Sidecars bzw. die Konfiguration.

### 16.9 Vollständiges Beispiel

Das folgende Beispiel wurde mit NHyDB 0.27.0 ausgeführt; die
Kommentare zeigen die tatsächlichen Ausgaben.

```python
import numpy as np
import nhydb
from nhydb.query import Q, QueryEngine

nhydb.n_to_dynadron_address(589)
# {'N': 589, 'N_ref': 'N589', 'realm': 1, 'u': 11, 'v_local': 13, 'v': 13, 'G': 24,
#  'lane': 1, 'lane_name': 'NR', 'NL': 565, 'NR': 589}

with nhydb.create("daten.nhydb") as db:      # legt .nhydb, .layout.enc, .layout.key, .lock an
    db.create_realm(1, "main")
    db.create_shape(1, 1, "row", [{"v": 1, "name": "id", "type": "varuint"},
                                  {"v": 2, "name": "status", "type": "utf8"},
                                  {"v": 3, "name": "temp", "type": "f64"}])
    db.create_classic_layout(1, 1, "records", shape_id=1)
    db.put_dense_batch(1, 1, [[u, "idle" if u % 3 else "active", 20.0 + u % 7]
                              for u in range(1, 1001)], shape_id=1)
    db.define_extension(1, 100, "utf8", name="note")
    db.put_extensions(1, 5, {"note": "hallo"})
    db.commit()

    db.get(1, 5)
    # {'_realm': 1, '_u': 5, '_lane': 0, '_lane_name': 'NL', '_shape': 1,
    #  'id': 5, 'status': 'idle', 'temp': 25.0, 'note': 'hallo'}

    db.put_cell(1, 5, 2, "rechts", type="utf8", lane="NR", commit=True)
    db.get_value(1, 5, 2), db.get_value(1, 5, 2, lane="NR")     # ('idle', 'rechts')

    db.create_index(1, 1, "idx_status", "status")
    db.build_indexes(commit=True)
    QueryEngine(db).execute(Q.field("status") == "active", realm=1,
                            return_mode="count")["count"]       # 333
    QueryEngine(db).execute((Q.field("status") == "idle") & Q.field("temp").between(21, 22),
                            realm=1, return_mode="fields", select=["id", "temp"],
                            limit=3)["rows"]
    # [{'record_N': 'N3', 'id': 1, 'temp': 21.0}, {'record_N': 'N7', 'id': 2, 'temp': 22.0},
    #  {'record_N': 'N73', 'id': 8, 'temp': 21.0}]

    db.create_realm(10, "np")
    db.create_shape(2, 10, "kq", [{"v": 1, "name": "N", "type": "varuint"},
                                  {"v": 2, "name": "tp", "type": "varint"}])
    db.create_classic_layout(12, 10, "kq", shape_id=2)
    u = np.arange(1, 100_001, dtype=np.uint64)
    db.put_numpy_batch(10, 1, {"N": u, "tp": u.astype(np.int64) * 2}, commit=True)
    db.read_numpy(10, 1, 5, ["N", "tp"])
    # {'N': array([1, 2, 3, 4, 5], dtype=uint64), 'tp': array([ 2,  4,  6,  8, 10]),
    #  '_present': array([ True,  True,  True,  True,  True])}

    db.verify(deep=True)["ok"]                                   # True

nhydb.NHyDB.compact_file("daten.nhydb")["ok"]                    # True
```

---

## 17. Konfiguration, Installation, Build und Betrieb

### 17.1 Voraussetzungen

| Komponente | Anforderung | Hinweis |
|---|---|---|
| Python | `requires-python = ">=3.9"` (`pyproject.toml`) | für das Python-Paket `nhydb` |
| NumPy | `numpy>=1.22` | optional (`nhydb[numpy]`), für die NumPy-Batch-API |
| Rust (Edition 2024, Resolver 3) | aktuelle stabile Toolchain (getestet mit Rust 1.99); mindestens Rust 1.89 | nur für den Build aus dem Quellcode; die Schreibsperre nutzt `std::fs::File::try_lock` (stabil seit Rust 1.89). Unter Windows zusätzlich die *Microsoft C++ Build Tools*, unter macOS die Xcode Command Line Tools |
| maturin | `maturin>=1.15,<2.0` | Build-Backend, nur für den Build aus dem Quellcode |
| pytest | Extra `nhydb[test]` = `pytest>=7`, `numpy>=1.22`, `cryptography` | nur für die Tests |

### 17.2 Konfigurationsdatei `nhydb_config.toml`

Eine kommentierte Beispieldatei liegt unter
`config/nhydb_config.example.toml`.

Suchreihenfolge (erste gefundene Quelle gilt):

1. API-Parameter `config=` (Pfad),
2. Umgebungsvariable `NHYDB_CONFIG`,
3. `nhydb_config.toml` im Verzeichnis der Datenbank,
4. eingebaute Standardwerte.

```toml
[database]
db_max_size_bytes = 53687091200   # 50 GiB; Startwert für NEUE Datenbanken
durability = "fsync"              # "fsync" | "full"

[math]
realm_auto_limit = 4096           # Startwert für NEUE MATH-Layouts
math_max_intermediate_bits = 1048576

[lock]
enabled = true                    # Sperrdatei <db>.lock
timeout_ms = 0                    # 0 = sofort Fehler, sonst Wartezeit in ms

[paths]
# layout_path = "{db}.layout.enc"           # {db} = Dateiname der Datenbank
# key_path    = "/sicherer/ort/{db}.layout.key"

[cache]
block_cache_bytes = 67108864
vindex_max_marks = 4000000
vindex_max_indexes = 4096
```

Regeln:

* Explizite API-Parameter (`layout_path`, `key_path`, `durability`, `lock`,
  `lock_timeout_ms`) haben Vorrang vor der Datei.
* In der Datenbank persistierte Werte (Größengrenze im `DB_CONFIG`-Block,
  `realm_auto_limit` je MATH-Layout) werden beim Öffnen **nie** überschrieben.
* Unbekannte Schlüssel sind Fehler; `realm_span`, `nl`, `nr` sind verbotene
  Schlüssel (feste Systemvorgaben).
* Relative Pfadvorlagen beziehen sich auf das Verzeichnis der Datenbank.
* **Hinweis zum Abschnitt `[cache]`:** Die Schlüssel werden geparst und
  validiert, in NHyDB 0.27.0 verwendet die Engine jedoch fest
  einkompilierte Werte (Block-Cache 64 MiB, v-Index 4 194 304 Sprungmarken
  und 4 096 Indizes). Siehe Abschnitt 19.1.

### 17.3 Installation und Build aus dem Quellcode

**Installation eines Wheels:**

```bash
pip install nhydb-0.27.0-<plattform>.whl
pip install numpy                             # optional, für die NumPy-API
```

**Build aus dem Quellcode** (Rust-Toolchain erforderlich, siehe `BUILDING.md`):

```bash
python3 -m venv .venv
source .venv/bin/activate            # Windows: .venv\Scripts\activate
pip install maturin
maturin build --release --out dist   # Wheel unter dist/
pip install dist/nhydb-0.27.0-*.whl
# alternativ Build und Installation in einem Schritt:
pip install .
```

Ein Wheel ist spezifisch für Betriebssystem, CPU-Architektur und
Python-Version, mit denen es gebaut wurde.

Tests aus dem Quellbaum:

```bash
cargo test --workspace --release     # Rust-Kern
pip install ".[test]"                # oder: pip install numpy pytest
python -m pytest tests/python        # Python-Paket (gegen das installierte Paket)
```

Die Python-Tests importieren das **installierte** Paket, nicht die Quellen
unter `python/`. Ausführliche Hinweise zu Toolchain, Plattformen und Wheels
enthält `BUILDING.md`. Die GitHub-Actions-Workflows unter
`.github/workflows/` führen die Tests aus (`ci.yml`) und bauen Wheels für
Linux, macOS und Windows (`release.yml`, ausgelöst durch einen Versions-Tag;
siehe `RELEASING.md`).

### 17.4 Plattformen

Betriebssystemspezifischer Code liegt ausschließlich in `platform.rs`
(Unix: `libc`, Windows: `windows-sys`). Dateiformat (Little-Endian,
UTF-8-JSON) und Layoutplan sind plattformneutral.

### 17.5 Betriebshinweise

* **Sicherung:** Immer alle drei Dateien sichern (`.nhydb`, `.layout.enc`,
  `.layout.key`); die Schlüsseldatei getrennt und zugriffsgeschützt.
* **Umbenennen:** Eine umbenannte DB-Datei öffnet nicht (R0,v2). Für eine
  Kopie unter neuem Namen `compact_file(path, output=neu)` verwenden.
* **Mehrere Prozesse:** genau ein öffnender Prozess je Datenbank (die
  Sperre wird bei jedem Öffnen exklusiv genommen); `lock_timeout_ms` wartet.
* **Durability:** `fsync` ist Standard; für Schutz gegen Laufwerkscache-Verlust
  auf macOS `durability="full"` wählen (im Kernbenchmark rund 30–60 %
  geringerer Schreibdurchsatz, Abschnitt 18.5).
* **Wartung:** regelmäßig `verify()`/`health()`; vor `repair_file(apply=True)`
  und Kompaktierung Kopien sichern (Backups sind standardmäßig aktiv).

---

## 18. Tests, Validierung und Benchmarks

### 18.1 Teststrategie

1. **Unit- und Integrationstests im Kern** (`cargo test --workspace --release`):
   Unit-Tests in `crates/nhydb-core/src/**` sowie die Integrationstests
   `crates/nhydb-core/tests/selftest.rs` und
   `crates/nhydb-core/tests/sparse_access.rs`.
2. **Python-Tests** (`python -m pytest tests/python`) über das installierte
   Paket `nhydb`.
3. **Erschöpfende Tests** (alle Unicode-Code-Points, Adressbereiche).
4. **System- und Wartungstests** (Sperren, Manipulation, Konfiguration,
   Durability, Repair, Kompaktierung).
5. **Funktionstests je Bereich** (Indizes, QueryEngine, NHySQL, MATH_MAP)
   mit erwarteten Ergebnissen, die unabhängig aus den Testdaten berechnet
   sind.

### 18.2 Testinventar

Rust (`cargo test --workspace --release`): 50 Unit-Tests in
`crates/nhydb-core/src/**`, 5 Tests in `selftest.rs`, 2 Tests in
`sparse_access.rs`; die PyO3-Crate `nhydb-py` enthält keine eigenen Tests.

| Datei | Schwerpunkt |
|---|---|
| `crates/nhydb-core/src/**` (Unit) | Adressmathematik, Nat/VarUInt, Format, Codec, Krypto (NIST-GCM-, RFC-4231-, SHA-KATs), R0-Kette, Plattform (Sperre, Sync, atomarer Austausch), Freigabe des Dateihandles bei `close`, Rücknahme nach Schreibfehler (Fehlerinjektion, die nur in Test-Builds existiert), Konfiguration, Mengenoperationen, Textindex, MATH, SQL-Lexer |
| `crates/nhydb-core/tests/selftest.rs` | End-to-End im Kern |
| `crates/nhydb-core/tests/sparse_access.rs` | Sparse-Punkt-/Bereichszugriff gegen unabhängiges Modell und Vollmerge (12 Seeds, `u = 2^70`, `v ≥ 10^40`) |
| `tests/python/test_math.py` (6) | Adressmathematik: Ablehnung von GZ/N0, Rechenbeispiel `(23, 14, 744)` |
| `tests/python/test_math_map.py` (5) | MATH-Ausdrücke, Abbildungsziele einer MathMap, MATH-Layout schreiben/lesen/wieder öffnen, Bitgrenze, Rollback von `put_math_batch` bei Größengrenze |
| `tests/python/test_index.py` (4) | Aufbau von VALUE-, TOKEN-, TRIGRAM- und FULLTEXT-Indizes, Gleichheits- und Bereichssuche, Token-/Teilstring-/Volltextkandidaten, Erkennung und Neuaufbau veralteter Indizes |
| `tests/python/test_iterators.py` (3) | Lazy-Iteratoren (`range`/`find`, Schreiben zwischen Iterationsschritten) |
| `tests/python/test_kernel.py` (8) | Kernelfunktionen, Unicode-16.0.0-Referenzwerte (alle Python-Versionen), Unicode-Tabellen gegen das laufende Python über alle Code Points (nur wenn es ebenfalls Unicode 16.0.0 verwendet, also CPython 3.14), AES-256-GCM gegen `cryptography`, Plan-MAC, JSON |
| `tests/python/test_kiss_api.py` (6) | Einzel-API, Kontextmanager, Durability-Option, Verlust unkommittierter Daten |
| `tests/python/test_numpy_api.py` (7) | NumPy ≙ Listen-API bytegleich, Dtypes, Endianness, Strides, Fehler ohne Teil-Append |
| `tests/python/test_numpy_columns.py` (5) | `put_math_numpy`, Abfrageergebnisse als NumPy-Spalten |
| `tests/python/test_query.py` (6) | QueryEngine: Zählen über einen frischen VALUE-Index, Aggregation und Gruppierung, Sortierung und Limit, BM25-Suche, Sparse-Überlagerungen über alle Zugriffspfade, große `STRUCT_RANGE` |
| `tests/python/test_sql.py` (8) | NHySQL: Filter, Gruppierung, Sortierung, positionelle und benannte Parameter, Volltext-Relevanz, NumPy-Ausgabe, Syntaxfehler und nicht unterstützte Anweisungen, Query-CLI |
| `tests/python/test_system.py` (6) | R0,u1, externer Layoutplan, fail closed, umbenannte DB, Sperre zwischen Prozessen, Konfiguration, Durability `full`, Repair |
| `tests/python/test_maintenance.py` (9) | Repair, Superblock-Fallback, Kompaktierung, Freigabe des Dateihandles bei `close`, zurückgenommene Appends, manipulierte Header, Plugin-Re-Entrancy, CLI |

Ein Lauf am 2026-10-18 (macOS, Python 3.14, mit `numpy` und
`cryptography`) ergab **73 bestandene Python-Testfälle** (gezählt
einschließlich Parametrisierungen) ohne übersprungene Tests, dazu 57
bestandene Rust-Tests. Die Unicode-Tabellen der Engine sind fest auf
Unicode 16.0.0 (CPython 3.14) festgelegt, unabhängig vom laufenden Python.
Unter CPython 3.9–3.13 (Unicode 13.0–15.1) werden deshalb die beiden
vollständigen Vergleiche gegen das laufende `unicodedata` übersprungen
(71 bestanden, 2 übersprungen); die festen Unicode-16.0.0-Referenzwerte
werden auf jeder Version geprüft. Unter Windows wird
`test_failed_append_is_rolled_back` übersprungen, weil er die
POSIX-Dateigrößengrenze (`RLIMIT_FSIZE`) benötigt; dieselbe Eigenschaft
(kein Teilblock nach einem Schreibfehler) prüft auf allen Plattformen der
Rust-Test `db::file_handle_tests::failed_append_is_rolled_back`. `test_kernel.py` überspringt den AES-GCM-Vergleich,
wenn das Paket `cryptography` fehlt; `test_sql.py` überspringt die
NumPy-Ausgabe ohne `numpy`.

### 18.3 Versionskennungen

| Kennung | Wert | Bedeutung |
|---|---|---|
| Produktversion | `0.27.0` | identisch in Cargo-Workspace (`nhydb-core`, `nhydb-py`), Python-Paket `nhydb` (Version über maturin aus Cargo) und `nhydb.ENGINE_VERSION` |
| Dateikennung | `NHYDB016` (`FILE_FORMAT_VERSION = "0.16"`, Blockversion 16) | interne Kennung des Dateiformats; ändert sich nur bei einer Änderung des Formats auf dem Datenträger, unabhängig von der Produktversion |
| `QUERY_VERSION` | `"0.27"` | Kennung der Abfrageschicht; erscheint als `query_version` in Abfrageergebnissen; keine Produktversion |
| `SQL_LANGUAGE_VERSION` | `"NHySQL-0.24"` | Kennung des Sprachumfangs von NHySQL; erscheint als `language` in SQL-Ergebnissen; keine Produktversion |
| Contract-Kennungen | `NHYDB027_R0U1_V1`, `NHYDB027_KEY_V1`, `NHYDB027_EXTERNAL_LAYOUT_V1`, `NHYDB027_SCHEMA_V1`, `NHYDB_DB_CONFIG_V015`, MAC-Domäne `NHYDB027-LAYOUT-MAC:` | feste Bezeichner der JSON-Strukturen bzw. der Schlüsselableitung im Dateiformat (Abschnitte 7.5 und 8.3); die enthaltenen Ziffern sind Teil des Bezeichners, keine Produktversion |

### 18.4 Lizenz

Siehe Datei `LICENSE.md`. NHyDB steht unter der „NHyDB / dNHyDB
Experimental Software License“, Version 1.0. Der Quelltext ist einsehbar,
es handelt sich aber nicht um eine Open-Source-Lizenz: Die kostenlose
Nutzung ist auf nicht-produktive Zwecke beschränkt (für Privatpersonen
unbefristet, für Lehre, Forschung und Unternehmen befristet), jede produktive Nutzung erfordert eine gesonderte
schriftliche Lizenz. Maßgeblich ist allein der Lizenztext. Anfragen an
achim.meissner@nhydb.de.

### 18.5 Benchmarks

**Messbedingungen:** gemessen am 2026-10-17 auf einem MacBook Air
(15 Zoll, Apple M2, 8 Kerne) unter macOS 26.6.2 mit Rust 1.99.0,
Release-Profil (Abschnitt 4.5), Datenbankdateien auf der internen SSD,
jeweils ein Einzellauf. Die Zahlen beschreiben nur die genannten
Szenarien auf diesem System; sie sind **keine** Leistungszusagen. Andere
Hardware, Dateisysteme und Datenmengen führen zu abweichenden Werten, und
Wiederholungen schwanken spürbar.

#### Kernbenchmark (reines Rust)

```bash
cargo run --release -p nhydb-core --example bench_native -- 1000000 <dir> fsync   # bzw. full
```

1 Mio. synthetische Datensätze mit 11 Ganzzahlfeldern („KQ-Datensätze“,
aus `N` abgeleitete Kennzahlen; Definition in
`crates/nhydb-core/examples/bench_native.rs`), Realm 500, Batch 10 000,
Commit alle 10 Batches, anschließend `verify`. Szenarien: BASE_ONLY (nur
Dense-Records), RESERVE_ONLY (CLASSIC-Layout mit `base_reserve_to = 1000`),
DEFINITIONS_EMPTY (zusätzlich fünf deklarierte, unbelegte Extensions),
SPARSE_MIX (zusätzlich 710 000 Sparse-Extension-Zellen).

| Szenario (1 Mio. Records) | Dateigröße | `fsync` | `full` |
|---|---:|---:|---:|
| BASE_ONLY | 23 375 280 B | 14,2 Mio. Records/s | 6,9 Mio. Records/s |
| RESERVE_ONLY | 23 375 304 B | 18,8 Mio. Records/s | 7,7 Mio. Records/s |
| DEFINITIONS_EMPTY | 23 375 328 B | 18,8 Mio. Records/s | 7,8 Mio. Records/s |
| SPARSE_MIX | 30 318 768 B | 5,7 Mio. Records/s | 4,1 Mio. Records/s |

Ausgabe des Laufs mit `fsync`:

```text
Scenario                     bytes   write s         rec/s    verify s
BASE_ONLY                 23375280     0.070      14188866       0.003
RESERVE_ONLY              23375304     0.053      18808587       0.003
DEFINITIONS_EMPTY         23375328     0.053      18805683       0.003
SPARSE_MIX                30318768     0.175       5722328       0.011
```

#### MATH im Kern

```bash
cargo run --release -p nhydb-core --example bench_math -- 200000 <dir>
```

200 000 Zeilen mit zwei Eingaben, 8 logische Realms, Payload-Typ `varint`,
`put_math_batch` mit Commit (Durability `fsync`), `get_math` für jede
zehnte Zeile.

| Operation | Durchsatz |
|---|---:|
| `MathMap.target` (Zielberechnung) | 1,77 Mio./s |
| `put_math_batch` | 852 000 Zeilen/s |
| `get_math` | 645 000 Abfragen/s |

## 19. Bekannte Einschränkungen

### 19.1 Einschränkungen

| Thema | Stand |
|---|---|
| Nutzdatenverschlüsselung | Nur der Layoutplan ist verschlüsselt; Daten in `X.nhydb` liegen im Klartext |
| Schlüsseldatei | ohne die passende `*.layout.key` lässt sich die Datenbank nicht öffnen (fail closed); ein verlorener Schlüssel kann nicht wiederhergestellt werden |
| Nebenläufigkeit | ein öffnender Prozess je Datenbank (exklusive Sperre `<db>.lock`, auch innerhalb eines Prozesses nur ein offenes Objekt je Datei); kein geteilter Lesemodus, keine Mehrprozess-Transaktionen |
| Transaktionen | keine explizite Transaktions-API; Atomarität über die Commit-Grenze; Mehrblock-Operationen nicht als Ganzes atomar (Ausnahme: `put_math_batch` mit Rollback) |
| Kontextmanager | `__exit__` committet auch nach einer Ausnahme im `with`-Block |
| Plugins | Rückrufe in dieselbe DB während eines Schreibvorgangs nicht unterstützt (`RuntimeError`); CUSTOM-Logik muss zur Laufzeit registriert werden |
| `[cache]`-Konfiguration | Schlüssel werden geparst und validiert, aber von der Engine noch nicht verwendet (fest: 64 MiB Block-Cache, 4 194 304 Sprungmarken, 4 096 v-Indizes); der Wert `vindex_max_marks = 4000000` im Konfigurationsbeispiel weicht vom einkompilierten Wert ab |
| Parameter `strict` | wird gespeichert, beeinflusst den Öffnungspfad in NHyDB 0.27.0 aber nicht; das Öffnen ist immer fail closed |
| Ältere Formatrevisionen | Dateien mit einer anderen Dateikennung als `NHYDB016` sind nicht lesbar; kein Konverter |
| ID-Bereiche | `shape_id`, `layout_id`, `index_id`, `block_id` ≤ 2^64−1; Enum-Codes `i64`; mathematische Koordinaten unbeschränkt |
| NumPy-Koordinaten | `u`/`v`/`object_id`-Arrays auf `uint64` begrenzt |
| Dense-Batch | Recordbereich ≤ 4 GiB je Block |
| Plan-Generationen | nur zwei Generationen im Plan; Superblock-Rückfall über mehr als eine Schema-Generation ⇒ fail closed |
| Repair | kann intakte, unkommittierte Blöcke dauerhaft machen |
| Durability `full` | macOS: `F_FULLFSYNC`, bei fehlender Unterstützung des Dateisystems ein Fehler (kein stiller Rückfall); Linux: `fsync(2)`; Windows: `FlushFileBuffers` (Abschnitt 9.6) |
| Plattformen | primär auf macOS (Apple Silicon) entwickelt und getestet; Wheels für Linux und Windows werden per CI gebaut und getestet |
| NHySQL | nur lesend, nur CLASSIC-Quellen; `DISTINCT`, `HAVING` nicht implementiert |
| Volltextsuche | `search` setzt einen frischen FULLTEXT-Index voraus (kein Scan-Fallback) |

### 19.2 Fehlertexte

Fehler aus intern verwendeten Bibliotheken erscheinen mit der passenden
NHyDB-Fehlerklasse und eigenem Text, z. B.
`CorruptionError("layout plan: decryption/authentication failed (invalid AES-GCM tag)")` bei einem
nicht entschlüsselbaren Layoutplan oder
`MathSyntaxError("invalid syntax (<unknown>, line 1)")` bei Syntaxfehlern in
MATH-Ausdrücken. Alle Fehlertexte der Engine sind englisch.

---

## 20. Anhang: Referenzen im Quellcode

| Thema | Datei |
|---|---|
| Adressmathematik | `crates/nhydb-core/src/address.rs`, `nat.rs` |
| Datentypen | `crates/nhydb-core/src/codec.rs`, `val.rs` |
| Containerformat | `crates/nhydb-core/src/format.rs`, `storage.rs`, `db/mod.rs` |
| R0,u1 und Layoutplan | `crates/nhydb-core/src/db/system.rs`, `crypto.rs` |
| Dense/Sparse | `crates/nhydb-core/src/db/dense.rs`, `db/sparse.rs`, `db/vindex.rs`, `db/interval.rs` |
| Layouts | `crates/nhydb-core/src/db/layout.rs`, `schema.rs`, `plugin.rs` |
| Lesen | `crates/nhydb-core/src/db/read.rs`, `db/materialize.rs`, `db/cursor.rs`, `db/columns.rs` |
| Indizes | `crates/nhydb-core/src/db/index.rs`, `textindex.rs`, `setops.rs`, `unicode.rs` |
| MATH | `crates/nhydb-core/src/mathexpr.rs`, `mathlayout.rs`, `db/math.rs` |
| Abfragen | `crates/nhydb-core/src/query.rs`, `sql.rs` |
| Wartung | `crates/nhydb-core/src/db/maint.rs` |
| Plattform, Konfiguration | `crates/nhydb-core/src/platform.rs`, `config.rs` |
| Python | `crates/nhydb-py/src/*.rs`, `python/nhydb/*.py` |
| Tests | `crates/nhydb-core/tests/*.rs`, `tests/python/test_*.py` |
| Beispielkonfiguration | `config/nhydb_config.example.toml` |
| Build | `BUILDING.md`, `pyproject.toml`, `Cargo.toml` |
