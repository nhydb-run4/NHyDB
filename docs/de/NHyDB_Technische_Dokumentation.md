# NHyDB 0.28.2 – Technische Dokumentation

| Metadatum | Wert |
|---|---|
| Dokument | Technische Referenzdokumentation NHyDB 0.28.2 (native Rust-Engine mit Python-Anbindung) |
| Sprache | Deutsch |
| Dokumentstand | 2026-11-11 – Engine und Dateiformat 0.28.2 (Änderungen gegenüber 0.28.0: `CHANGELOG.md`, Abschnitt `[0.28.2]`) |
| Produkt | NHyDB 0.28.2, Python-Paket `nhydb` |
| Engine-Version (`ENGINE_VERSION`) | `0.28.2` |
| Dateiformat (`FILE_FORMAT_VERSION`) | `0.28.2` – die Formatversion ist das Engine-Release, das das Format festgelegt hat; Signatur `NHYDB` + Versionsbytes 0, 28, 2. Dateien von NHyDB 0.28.0 (`NHYDB016`, Format 0.16) und Datenbanken von NHyDB 0.27.1 oder älter werden nicht gelesen (`CorruptionError` mit Nennung des Formats, Abschnitt 7) |
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
7. [Speicherorganisation und Dateiformat 0.28.2](#7-speicherorganisation-und-dateiformat-0282)
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
| Persistenz | Append-only-Container, alternierende Superblöcke, Commit-Grenze, Tail-Recovery (nie auf Kosten committeter Daten), Durability `fsync`/`full`, Lesemodus `readonly=True` |
| Sicherheit | Systemadressraum R0,u1, verschlüsselter externer Layoutplan (AES-256-GCM, HMAC-SHA256), fail-closed Öffnen, Schreibsperre, DB-Größengrenze |
| Indizes | VALUE, TOKEN, TRIGRAM, FULLTEXT (Positionen, BM25); persistente, sortierte Schlüsselbäume mit Seiten-Cache, Aufbau mit externer Sortierung |
| MATH_MAP | sichere Ausdruckssprache, Abbildung von Eingabetupeln auf Zellen, Realmplaner, exklusive MATH-Realms |
| Abfragen | QueryEngine (Kostenplaner, Provider, Aggregation, Ranking) und NHySQL-0.24 (lesendes `SELECT`) |
| Wartung | Checkpoint, Verify, Health, Stats, Repair, Kompaktierung (in-place und Export), Wiederherstellung von Sicherungen |
| Python | Python-API (Einzeloperationen) und NumPy-Batch-API |

### 1.3 Abgrenzung

NHyDB ist **kein** relationales Datenbanksystem, **kein** Server und
**kein** Mehrprozess-Transaktionssystem. Es gibt genau einen schreibenden
Prozess je Datenbank (abgesichert durch eine Sperrdatei), keine
Netzwerkschnittstelle und keine Transaktions-API mit `BEGIN`; `rollback()`
verwirft lediglich die unkommittierten Änderungen eines Objekts
(Abschnitt 9.7). NHySQL ist eine lesende Abfragesprache; Schreibzugriffe
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
Koordinaten (zu Grenzen durch Ressourcen und Konfiguration siehe Abschnitt 3.6).

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

Drei Ebenen sind zu unterscheiden:

| Ebene | Bedeutung | Größe |
|---|---|---|
| logischer Realm `R_log` | Ergebnis der Zerlegung von `V`; Teil der Mathematik des Layouts | unbeschränkt (z. B. ≈ 10^5000 möglich) |
| physischer Realm `R_phys` | vom Realmplaner vergebene, dicht ab 1 belegte Realm-ID, unter der die Zelle gespeichert wird; die Zuordnung `R_log → R_phys` steht im verschlüsselten Layoutplan | klein (Anzahl belegter Realms) |
| `N` der gespeicherten Zelle | `N(R_phys, u, v, side)` | – |

Die riesige logische Realmnummer wird nie als Zellkoordinate gespeichert.
Das physische `N` einer MATH-Zelle stimmt nur dann mit dem „mathematischen“
`N(R_log, u, v, side)` überein, wenn `R_log` auf denselben physischen Realm
abgebildet wird (z. B. `R_log = 1 → R_phys = 1`). `get_math_cell` liefert
`_logical_realm` und `_V_global` zusätzlich zur physischen Adresse.

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

### 3.6 Große Zahlen: BigInt-Fähigkeit und Grenzen

Adressierung, Speicherung und Wiederöffnen sind durchgängig für beliebig
große natürliche Zahlen ausgelegt: `R`, `u`, `V`, `N` sowie Werte der Typen
`varuint`/`varint` werden exakt übernommen (Python ↔ Rust binär über
`BigInt`, ohne Zwischenstrings), gespeichert und nach dem Wiederöffnen
gelesen. `get_vu` hat **keine feste Bitgrenze**; die Länge einer VarUInt ist
nur durch die verbleibenden Bytes des CRC-geprüften Block-Payloads begrenzt
(abgeschnittene Werte ⇒ `CorruptionError("VarUInt ends prematurely")`).
Allokationen werden mit `try_reserve` angefordert; fehlt Speicher, gibt es
einen sauberen `MemoryError` statt eines Absturzes. Getestet sind Koordinaten
und Werte mit 2^16 380, 2^16 400, 10^5 und 10^6 Bit über Einzel-API,
`put_points`, `create_realm`, Dense-Batches und MATH-Durchreichung,
jeweils mit Commit, Wiederöffnen, Lesen und `verify(deep=True)`
(`tests/python/test_stab_bigint.py`).

| Art | Grenze | Wirkungsbereich |
|---|---|---|
| Mathematisch (Dynadron) | `u, v, R ≥ 1`; GZ/N0 ausgeschlossen; `v_local ≤ 10^100` je Realm | Teil des Konzepts |
| Konfiguration | `realm_auto_limit` (**Anzahl** der logischen Realms eines MATH-Layouts, nicht deren Größe) | nur MATH-Layouts (Abschnitt 13.4) |
| | `math_max_intermediate_bits` | nur Rechenoperationen in Formeln (Abschnitt 13.2), nie durchgereichte Eingaben |
| | IDs (`shape_id`, `layout_id`, `index_id`, `default_shape_id`) ≤ 2^63 − 1 | nur Verwaltungs-IDs (`ValueError`) |
| Technisch | NumPy-Batch-APIs: Koordinaten-Arrays `uint64` | nur NumPy-Pfad; Einzel- und Listen-APIs sind unbeschränkt |
| | Python-Anzeige: `str(int)` mit mehr als 4 300 Dezimalstellen verboten | nur Ausgabe in Python (`print`, `json`, f-Strings) |
| Physisch | Arbeitsspeicher, Platte (`db_max_size_bytes`), Rechenzeit | eine Adressberechnung mit 10^6 Bit dauert ≈ 0,1 s |

**Anzeige sehr großer Zahlen in Python:** Seit Python 3.11 (auch in
Sicherheitsupdates älterer Versionen) lehnt
`str(int)` Zahlen mit mehr als 4 300 Dezimalstellen ab (`ValueError:
Exceeds the limit (4300 digits) for integer string conversion`). Das betrifft
nur die Darstellung, nicht NHyDB: Werte kommen als `int` zurück, und
Adress-Dicts enthalten zusätzlich `N_ref` als Text. Für die Ausgabe
`sys.set_int_max_str_digits(0)` setzen (0 = keine Grenze) oder mit
`int.bit_length()` arbeiten.

```python
import sys
sys.set_int_max_str_digits(0)      # nur für die Anzeige riesiger Zahlen
```

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
        STORE["dense · sparse · vindex · interval · directory · segindex · merge"]
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
für rein native Abschnitte frei (Abfragen, SQL, Batch-Schreiben, Commit,
Wartung) und überbrückt Python-Plugins und -Provider, die den GIL für ihren
Rückruf zurückholen. Lesende Aufrufe teilen sich ein `NHyDB`-Objekt
(mehrere Threads dürfen gleichzeitig abfragen); ein schreibender Aufruf
braucht das Objekt exklusiv, ein Konflikt wird sofort abgewiesen
(Abschnitt 16.7, „Threads“).

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
| `query.rs` | 4 276 (0.28.2) | QueryEngine: Ausdrucksbaum, Planer, Auswertung, Aggregation, Suche |
| `sql.rs` | 1 399 (0.28.2) | NHySQL: Lexer, Parser, Compiler |
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
| `db/index.rs` | 1 155 | Indexaufbau, Postings, Kandidaten |
| `db/math.rs` | 553 | MATH-API im Kern, Rollback |
| `db/maint.rs` | 1 060 | Checkpoint, Verify, Health, Stats, Repair, Kompaktierung |
| `db/blockcache.rs` | neu in 0.28.0 (609) | Block-Cache (LRU), „verified“-Register, Teil-Lesen geprüfter Blöcke, Cache-Budgets (Abschnitt 9.8) |
| `db/batch.rs` | neu in 0.28.0 (116) | Prüfung einzelner Zellen für `batch()`, `rollback()` |
| `db/directory.rs` | neu in 0.28.2 (1 386) | persistentes Blockverzeichnis: Seiten, Läufe, Wurzel, Flush und Zusammenführung (Abschnitt 7.10) |
| `db/segindex.rs` | neu in 0.28.2 (569) | Segmentverwaltung je Realm/Lane aus Verzeichnisseiten und Tail |
| `db/merge.rs` | neu in 0.28.2 (1 099) | speicherbegrenzte Zusammenführung der Sparse-Blöcke (Kompaktierung, Iteratoren), externe Läufe |
| `db/secindex.rs` | neu in 0.28.2 (1 201) | SECONDARY-Schlüsselbäume: Schlüsselkodierung und -ordnung, Seiten, Suche, Bereichsdurchlauf, Seiten-Cache (Abschnitt 7.8) |
| `db/idxspill.rs` | neu in 0.28.2 (385) | externe Sortierung des Indexaufbaus: sortierte Läufe in einer anonymen temporären Datei, schlüsselweises Mischen über einen Heap (Abschnitt 12.2) |
| `db/mempot.rs` | neu in 0.28.2 (497) | Prozessbudget aller Caches (`[cache] total_bytes`), Bytebudget-Maps, Verdrängung über die Handles (Abschnitt 9.8) |
| `container.rs` | neu in 0.28.2 (732) | Ergebniscontainer `.nhyc`: CRC-32C-geschützte Chunks, gesperrtes Schreiben, atomares Abschließen ohne Überschreiben, anonyme Sortierläufe (Abschnitt 14.5) |
| `valcodec.rs` | neu in 0.28.2 (298) | kanonischer Wertecodec der Container (Abschnitt 14.5) |
| `results.rs` | neu in 0.28.2 (487) | Ergebnisdateien, Ergebnisverzeichnis und Budget, `cleanup_results`, Hintergrundaufträge (Abschnitt 14.5) |

In 0.28.2 wurden außerdem `db/vindex.rs` (773 Zeilen) und `db/interval.rs`
(481 Zeilen) neu gefasst (Abschnitt 9.4).

### 4.3 PyO3-Schicht `nhydb-py`

| Datei | Aufgabe |
|---|---|
| `lib.rs` | Klasse `NHyDB`, `LayoutDef`, `IndexDef`, Modulfunktionen, Registrierung |
| `conv.rs` | Konvertierung Python ↔ `Val`/`Nat` (beliebig große `int`) |
| `exceptions.rs` | Ausnahmehierarchie |
| `numpy_io.rs` | Buffer-Protokoll für NumPy-Arrays, Ausgabe ohne Kopie über `ColumnBuffer` und `numpy.frombuffer` |
| `batch.rs` | `WriteBatch` für `NHyDB.batch()` |
| `iter.rs` | `ResultIterator` (lazy, blockweise) |
| `pyplugin.rs` | Brücke für Python-CUSTOM-Plugins (Weak-Referenz auf das DB-Objekt) |
| `kernel.rs` | zustandslose Kernelfunktionen für `nhydb.kernel` |
| `textapi.rs` | Funktionen für `nhydb.index` |
| `mathapi.rs` | Klassen für `nhydb.math`/`nhydb.mathlayout`, MATH-Methoden |
| `queryapi.rs` | native `QueryEngine`, SQL, Provider-Brücke |

### 4.4 Externe Abhängigkeiten

Versionen laut `Cargo.lock` von NHyDB 0.28.0:

| Crate | Version | Verwendung |
|---|---|---|
| `num-bigint` / `num-integer` / `num-traits` | 0.4.8 / 0.1.47 / 0.2.19 | exakte Ganzzahlarithmetik |
| `crc32fast` | 1.5.2 | CRC32 für Superblöcke, Blockheader, Payloads |
| `miniz_oxide` | 0.8.9 | zlib-Dekompression; Kompression unter Windows |
| `aes-gcm` | 0.11.1 | AEAD-Verschlüsselung des Layoutplans |
| `sha2` | 0.11.1 | SHA-256 (Fingerprints, Schlüsselableitung für MAC und Schema-Digest) |
| `hmac` | 0.13.0 | HMAC-SHA256 über den Planumschlag und den Schema-Snapshot |
| `base64` | 0.23.1 | Kodierung von Schlüssel, Nonce und Ciphertext |
| `getrandom` | 0.4.3 | Zufallszahlen des Betriebssystems (Schlüssel, Nonces, UUID) |
| `toml` | 1.1.8 | Konfigurationsdatei |
| `libc` (nur Unix) | 0.2.190 | `fsync`, `fcntl(F_BARRIERFSYNC/F_FULLFSYNC)`, `O_NOFOLLOW`, `sysconf` (physischer Arbeitsspeicher), `localtime_r` |
| `windows-sys` (nur Windows) | 0.61.2 | `GlobalMemoryStatusEx` (physischer Arbeitsspeicher), `GetLocalTime`; Datei-Sync (`FlushFileBuffers`) und Umbenennen über die Standardbibliothek |
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

Dazu `.cargo/config.toml` mit `rustflags = ["-C",
"llvm-args=-align-all-functions=6"]`: Funktionsanfänge auf 64 Byte
ausgerichtet. Ohne sie verschob die Lage unbeteiligten Codes den
KQ-10M-Import um etwa 6 %. Die Datei gehört zum Release-Export. Gemessen im
Wheel aus dem exportierten Baum: 99,9 % der Funktionen ausgerichtet, ohne
die Datei 7,6 %. Eine gesetzte Umgebungsvariable `RUSTFLAGS` ersetzt diese
Einstellung.

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

Alle Schemaobjekte liegen (seit Format 0.16, unverändert in 0.28.2) **nicht** in der Datenbankdatei,
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

**Neuester Schreibzustand gewinnt – über beide Schichten** (seit 0.28.2).
Für jede Zelle gilt der zuletzt geschriebene Zustand, gleich ob er als
Dense-Record oder als Sparse-Zelle geschrieben wurde. Die Reihenfolge ist die
Block-ID; sie liegt in beiden Strukturen vor, das Format ist unverändert:

* Ein Dense-Record (Block-ID `d`) enthält alle Felder seines Shapes. Ein
  Sparse-Zustand eines dieser Felder aus einem **älteren** Block (`s < d`)
  ist durch den Record verdeckt; ein neuerer (`s > d`) überlagert ihn.
  Beispiel: `put(x=10)`, `put_cell(x=11)`, `put(x=12)` liest `x = 12`;
  `put`, `delete_cell(x)`, `put(x=20)` liest `x = 20`.
* Sparse-Zustände an `v` außerhalb des Shapes (Extensions, reservierte
  BASE-Positionen, Felder anderer Shapes) verdeckt ein Dense-Record nie: ein
  `put` schreibt nur die Felder seines Shapes.
* **`put` ersetzt den BASE-Record als Ganzes.** Ersetzt ein `put` mit Shape B
  einen Record mit Shape A, gehören die Felder von A, die B nicht hat, nicht
  mehr zum Record. War ein solches Feld vorher als Sparse-Zelle geschrieben
  und vom A-Record verdeckt, schreibt `put` dafür in derselben Einheit einen
  Tombstone, damit der überholte Wert nicht wieder sichtbar wird
  (`put_cell(y=7)`, `put(A{x, y=20})`, `put(B{x})` → `y` existiert nicht).
  Diese Prüfung läuft nur, wenn die Lane Sparse-Zellen und Dense-Records
  eines Shapes mit zusätzlichen Feldern hat; sonst liest der Schreibweg nichts.
* Ein `DELETE`-Block verdeckt den Dense-Record von u (höhere Block-ID);
  `delete_record`/`delete_area` schreiben zusätzlich Tombstones für alle
  Sparse-Werte der u-Gruppe, auch die vom Record verdeckten – ein `DELETE`
  allein ließe sie wieder sichtbar werden.

Dieselbe Regel wenden alle Lese- und Wartungswege an: `get`/`get_internal`,
`get_cell`/`get_value`, `read_numpy`, Zellen-Cursor
(`iter_materialized_cells`, SQL/QueryEngine), `iter_materialized_sparse_points`,
Gewinner-Sicht (`[query] overlay_view`), Indexaufbau und inkrementelle
Indexpflege und die Kompaktierung. Die Kompaktierung verwirft verdeckte
Sparse-Zustände und schreibt die überlebenden in der Zieldatei **nach** dem
Dense-Record ihres u (höhere Block-ID), auch über Paketgrenzen hinweg.

Bis 0.28.0 galt „Sparse liegt immer oben“: ein älteres `put_cell` oder
`delete_cell` verdeckte ein späteres `put` desselben BASE-Feldes (so auch in
Python v0.14/v0.27). Das ist seit 0.28.2 als Fehler korrigiert.

**Existenz einer Zelle.** Eine Zelle lebt, wenn ihr neuester Zustand ein
Wert ist – ein gespeichertes `None` eingeschlossen. Ein Tombstone, eine nie
geschriebene Zelle und ein Feld des aktuellen Shapes, das der gespeicherte
Record nicht hat, existieren nicht (`get_cell(...)["present"] = False`).
Ein Datensatz existiert, solange er mindestens eine lebende Zelle hat
(SQL, QueryEngine, `read_numpy`-Spalte `_present`; die N-Pfade `get_n`,
`get_n_cell`, `get_address` und `describe_n` melden `present`/`occupied`
nach derselben Regel wie `get_cell`). `get()`, `range()` und `select()`
zeigen die Datensatzsicht von v0.14 (Records mit lebendem Dense-Record).
Dense-Felder sind typisiert und können kein `None` speichern: ein `None` in
einem Feld des Shapes ergibt für jeden Feldtyp `ValueError` (bis 0.28.0
speicherten `utf8`-Felder still `"None"`, `bool`-Felder `False`);
ein `None` entsteht nur über `put_cell`.

**Löschen.** Die öffentliche Lösch-API hat genau drei Funktionen. Sie
arbeiten auf logischen Zellen, unabhängig von BASE/Extension, Dense/Sparse
und Feldnamen:

| Befehl | Wirkung |
|---|---|
| `delete_cell(realm, u, v)` | Tombstone für eine lebende Zelle (auch ein BASE-Feld eines Dense-Records oder eine Null-Zelle). Rückgabe `True`, wenn sie lebte; sonst `False` ohne Schreibvorgang |
| `delete_area(realm, u_from, u_to, v_from, v_to)` | alle lebenden Zellen im Rechteck (Grenzen inklusiv): je u ein `DELETE`-Block plus Tombstones der Sparse-Werte, wenn alle lebenden Zellen des u im Bereich liegen, sonst Tombstones je Zelle. Rückgabe: Zahl der gelöschten lebenden Zellen. Eine Einheit (`write_scope`); Tombstones in Paketen zu 8 192; Aufwand proportional zu den lebenden Zellen |
| `delete_record(layout_id, object_id)` | alle lebenden Zellen des Objekts: CLASSIC – `DELETE`-Block und Tombstones aller Sparse-Werte der u-Gruppe; COLUMN/PACKED/CUSTOM – Tombstones der lebenden Zellen, die das Layout dem Objekt zuordnet. Rückgabe: Zahl der gelöschten lebenden Zellen (0 = nichts, dann kein Schreibvorgang) |

`delete(realm, u)` und `delete_point(realm, u, v)` sind seit 0.28.2
entfallen; `delete` verdeckte nur den Dense-Record und ließ
Einzelzellen und Extensions stehen. Intern bleiben der `DELETE`-Block und
der Sparse-Tombstone die beiden physischen Formen.

Gelöschte Daten bleiben als ältere Blöcke in der Datei (append-only).
Physisch entfernt werden verdeckte Dense-Records und gelöschte Zellen erst
durch `compact_file()` (Abschnitt 15.5).

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
  (`VarUInt ends prematurely`, `length value ends beyond the payload`,
  `fixed-width value incomplete`). Eine feste Bitgrenze gibt es nicht
  (Abschnitt 3.6).

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

## 7. Speicherorganisation und Dateiformat 0.28.2

Die Engine schreibt das Dateiformat 0.28.2 (`FILE_FORMAT_VERSION = "0.28.2"`).
**Die Formatversion ist das Engine-Release, das das Format festgelegt hat**;
die historische Zählung `NHYDB016`/`NHYDB017` endet. Die Datei beginnt mit
der Signatur `NHYDB` und den drei Versionsbytes 0, 28, 2 (Major, Minor,
Patch). Gegenüber dem Format 0.16 von NHyDB 0.28.0 kommen das
**persistente Blockverzeichnis** (Blocktyp 14 `DIRECTORY`, Abschnitt 7.10),
das Superblockfeld `dir_root`, eine **Präfixprüfsumme** in DATA_DENSE- und
SPARSE_CELLS-Payloads (Tags `D28D`/`SP28`, Abschnitte 7.6/7.7) und
SECONDARY-Indizes als **Schlüsselbäume** (Tag `SI28`, Blocktyp 15
`SECONDARY_PAGE`, Abschnitt 7.8) hinzu; alle übrigen Blöcke und Payloads
sind unverändert (Golden-Test `test_stab_format_golden.py`). Alle Ganzzahlen
sind Little-Endian.

**Kompatibilität mit NHyDB 0.28.0.** Dateien im Format 0.16 werden
nicht gelesen (keine Rückwärtskompatibilität in der Entwicklungsphase): Das
Öffnen scheitert mit `CorruptionError` „unsupported file format NHYDB016
(expected NHYDB 0.28.2)“, die Datei bleibt unverändert. Blöcke des Formats
0.16 (`D10D`, `SP10`, `SI10`) gelten als falsches Tag. Ein unveröffentlichtes
Entwicklungsformat (`NHYDB017`) und ältere Formatrevisionen werden
ebenfalls nicht gelesen. Einen Migrationsweg oder Konverter gibt es nicht:
die Datenbank aus den Quelldaten bzw. Exporten mit 0.28.2 neu aufbauen.

**Kompatibilität mit NHyDB 0.27.1.** Dateikennung `NHYDB016`, Superblöcke,
Blockheader und Daten-Payloads waren in 0.28.0 gegenüber 0.27.1 unverändert. Seit 0.28.0
ist der Schema-Digest im Layoutplan (Feld `sha256`) und in R0,u1,v6 jedoch ein
HMAC-SHA256 mit einem aus dem Schlüssel abgeleiteten Schlüssel (Abschnitt 8.3)
statt eines einfachen SHA-256 des Schema-Snapshots; außerdem enthält der
MATH-Layoutzustand das Feld `max_intermediate_bits`. Datenbanken, die mit
NHyDB 0.27.1 oder älter angelegt wurden, öffnen nicht – auch nicht mit
`readonly=True`: Eine solche Datei scheitert bereits am Superblock
(`CorruptionError` „unsupported file format NHYDB016 …“, siehe oben); die
Dateien bleiben unverändert. Die Prüfung des Layoutplans auf einen
Schema-Digest ohne Schlüssel besteht weiter: Ein solcher Plan ergibt
`CorruptionError`, dessen Text NHyDB 0.27.1 und den Weg über Export und
Neuimport nennt (`repair_file`: `action: "abort"` mit `plan_error`). Eine
Migration gibt es nicht: Die Daten mit NHyDB 0.27.1
auslesen (exportieren), mit 0.28.2 eine neue Datenbank anlegen, die Daten
dort importieren und die Indizes mit `build_indexes()` aufbauen.

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
                       | DIRECTORY (Seiten, Lauf, Wurzel – Abschnitt 7.10)
                       | SECONDARY_PAGE (Blatt, innere Seite, Posting-Block – Abschnitt 7.8)
              ── Commit-Grenze last_valid_end ──
              (optional: unkommittierter Tail, wird beim Öffnen abgeschnitten,
               sofern die Commit-Grenze eindeutig ist – Abschnitt 8.4)
```

### 7.3 Superblock

| Offset | Typ | Inhalt |
|---:|---|---|
| 0 | 8 Byte | `NHYDB` + Bytes `00 1C 02` (0.28.2; andere Signaturen ⇒ `CorruptionError`) |
| 8 | u16 | Major `0` |
| 10 | u16 | Minor `28`; der Patch steht in der Signatur |
| 12 | u32 | Superblockgröße `4096` |
| 16 | u64 | `generation` |
| 24 | u64 | `checkpoint_offset` |
| 32 | u64 | `last_valid_end` (Commit-Grenze) |
| 40 | u64 | `next_block_id` |
| 48 | u64 | `created_ns` |
| 56 | u64 | `updated_ns` |
| 64 | u32 | `flags` |
| 68 | u64 | `dir_root`: Offset der Wurzel des committeten Blockverzeichnisses (0 = keins) |
| 4092 | u32 | CRC32 über Bytes 0 … 4091 |

Beim Öffnen gewinnt der gültige Superblock mit der höchsten Generation,
sofern seine Commit-Grenze in der Datei liegt (Bewertung beider Slots:
Abschnitt 8.4). Ein Commit schreibt stets den **anderen** Slot. Erst ein
Superblock mit `dir_root` macht ein Verzeichnis gültig.

### 7.4 Blockheader

32 Byte, Struktur `<4sBBHQQII`:

| Feld | Typ | Inhalt |
|---|---|---|
| Magic | 4 Byte | `NHBL` |
| Version | u8 | `16` (Blockversion, im Format 0.28.2 unverändert) |
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
| 4 | DATA_DENSE | DB | `D28D vu(realm) vu(shape) vu(u_start) vu(count) u32(präfixprüfsumme) u32[count+1] records` |
| 5 | DELETE | DB | `vu(realm) vu(u)` |
| 6 | CHECKPOINT | DB | `CP10` + JSON |
| 7 | SECONDARY | DB | Wurzel eines Schlüsselbaums `SI28 vu(index) vu(source_block) vu(keys) vu(höhe) vu(len) zlib(oberste Seite)` |
| 8 | EXTENSION_DEF | nur Layoutplan | – |
| 9 | LAYOUT_DEF | nur Layoutplan | – |
| 10 | SPARSE_CELLS | DB | `SP28 vu(realm) vu(u_start) vu(u_end) vu(groups) vu(cells) u32(präfixprüfsumme) stream` |
| 11 | MATH_LAYOUT_STATE | nur Layoutplan | – |
| 12 | DB_CONFIG | DB | JSON `{"contract":"NHYDB_DB_CONFIG_V015","max_db_size_bytes":N}` |
| 13 | SYSTEM (R0,u1) | DB | JSON `{"contract":"NHYDB027_R0U1_V1","cells":{…}}` |
| 14 | DIRECTORY | DB | Seite `DP17`, Lauf `DR17` oder Wurzel `DT17` des Blockverzeichnisses (ab 0.28.2, Abschnitt 7.10) |
| 15 | SECONDARY_PAGE | DB | Blatt `SL28`, innere Seite `SN28` oder Posting-Block `SC28` eines Schlüsselbaums (ab 0.28.2, Abschnitt 7.8) |

Ein Layout-Metablock (Typen 1, 2, 3, 8, 9, 11) in der Datenbankdatei ist ein
Fehler; das Öffnen schlägt mit `CorruptionError` fehl (fail closed).

**Implementierungsdetail:** Schemaoperationen rufen intern weiterhin den
Append-Pfad mit diesen Blocktypen auf. `Db::append_block_limit` fängt sie ab,
schreibt nichts in die Datei und setzt stattdessen `layout_dirty`, sodass der
nächste Commit eine neue Plangeneration erzeugt.

### 7.6 Dense-Block

```text
"D28D"
vu(realm) vu(shape_id) vu(u_start) vu(count)
u32                     Präfixprüfsumme: CRC-32C über die Bytes ab "D28D" bis vor dieses Feld
u32[count + 1]          Offsets relativ zum Recordbereich (letzter = Gesamtlänge)
records                 Record i = Bytes [off[i], off[i+1])
```

Ein Record enthält die BASE-Felder in Shape-Reihenfolge (feste Typen LE,
`varuint`/`varint`, `utf8`/`bytes` als `vu(len)` + Bytes, `bool` 1 Byte).
Ein optionaler EXT-Bereich
`0xEA vu(count) (vu(Δv<<1 | has_value) [wert])*` wird beim Lesen
unterstützt, aber nicht geschrieben. Wegen der 32-Bit-Offsets darf der Recordbereich eines
Blocks höchstens 4 GiB umfassen
(`ValueError("a DATA batch may contain at most 4 GiB of record data")`).
Ab 0.28.2 teilt ein dichter Schreibaufruf (`put_dense_batch`, `put_numpy_batch`,
Kompaktierung) seine Datensätze in aufeinanderfolgende DATA-Blöcke von höchstens
`[write] max_block_bytes` Payload (Standard 8 MiB, 0 = ein Block je Aufruf);
ein einzelner größerer Datensatz erhält einen eigenen Block. Alle Blöcke eines
Aufrufs bilden eine Einheit (Schreibklammer), jeder Block wird kodiert,
angehängt und freigegeben, bevor der nächste entsteht. Das Ergebnis behält
seine Felder: `block_offset`/`block_id` bezeichnen den ersten Block, `bytes`
die Payload aller Blöcke.

### 7.7 Sparse-Block

```text
"SP28"
vu(realm) vu(u_start) vu(u_end) vu(groups) vu(cells)
u32                     Präfixprüfsumme: CRC-32C über die Bytes ab "SP28" bis vor dieses Feld
stream:  je u-Gruppe:  vu(Δu) vu(len) record        (erstes Δu = 0)
record:  vu(count), dann je Zelle aufsteigend nach v:
         vu(Δv << 3 | flags)  [u8 typcode, falls selbsttypisiert]  [wert, außer Tombstone/Null]
flags:   1 = Tombstone, 2 = selbsttypisiert, 4 = Null
```

`v` muss je `u` streng aufsteigend sein (`SchemaError`). Schreibvorgänge
gruppieren Zellen in `u`-Fenstern (Standard `u_window = 10 000`, MATH:
`10^8`) zu je einem Block. Die Kompaktierung teilt Sparse-Blöcke seit 0.28.2
nicht mehr nach einem `u`-Fenster (weniger, größere Blöcke, Abschnitt 15.5).

**Präfixprüfsumme (ab 0.28.2, Dense- und Sparse-Block).** Das Präfix eines
Datenblocks (Tag bis einschließlich der Zähler) leitet beim Öffnen die
`u`-Bereiche zu den Blöcken; gelesen wird dabei nur das Präfix, nicht die
ganze Payload. Dahinter steht deshalb eine eigene Prüfsumme: `u32` (LE) =
**CRC-32C** (Castagnoli, reflektiertes Polynom `0x82F63B78`) über die
Präfixbytes ab dem Tag (`format.rs`: `prefix_crc`, `check_prefix_crc`).
Bewusst nicht die CRC32 der Payload: Hängt man an Daten ihre eigene CRC32
an, ist die CRC32 über beides konstant – die Payload-CRC hinge dann nicht
mehr vom Präfix ab. Mit dem zweiten Polynom bleiben beide Prüfungen
unabhängig: Ein Präfix, dessen Prüfsumme neu berechnet wurde, scheitert
weiter an der Payload-CRC. Wann welche Prüfung erfolgt, zeigt Abschnitt
15.1.

### 7.8 SECONDARY-Block (Index)

Seit 0.28.2 ist jeder Index (je Lane) ein **persistenter, sortierter
Schlüsselbaum** (von unten aufgebauter B+-Baum, `db/secindex.rs`) mit
unkomprimiert etwa 4 KiB großen Seiten. Jede Seite ist ein eigener Block.
Keine Abfrage lädt alle Schlüssel in den Arbeitsspeicher.

| Teil | Blocktyp | Payload |
|---|---:|---|
| Wurzel | 7 SECONDARY | `SI28 vu(index) vu(source_block) vu(keys) vu(höhe) vu(len) zlib(oberste Seite)` – bei Höhe 0 ist die eingebettete Seite das einzige Blatt |
| Blatt | 15 SECONDARY_PAGE | `SL28 vu(index) vu(0) vu(rohlänge) zlib(körper)`, Körper `vu(m) (vu(off) vu(block_id))^m vu(n) eintrag^n`; Eintrag `vu(klen) schlüssel vu(count) u8(codec) u8(ort) …` mit Ort 0 = Posting inline (`vu(len) bytes`, bis 256 Byte) oder 1 = Posting-Block (`vu(i) vu(rel) vu(clen)`: Index in die Blocktabelle des Blatts, Payload-Offset und Länge der zlib-komprimierten Posting) |
| Innere Seite | 15 SECONDARY_PAGE | `SN28 vu(index) vu(stufe) vu(rohlänge) zlib(vu(n) (vu(klen) erster_schlüssel vu(off) vu(block_id))^n)` |
| Posting-Block | 15 SECONDARY_PAGE | `SC28 vu(index)` + aneinandergereihte zlib-Postings (Ziel 1 MiB; eine größere Posting erhält einen eigenen Block) |

**Schlüssel** sind binär kodiert: `0x00` None, `0x01`/`0x02` False/True,
`0x03` vu(zigzag i64), `0x04` große Ganzzahl (Zweierkomplement LE), `0x05`
f64 (LE), `0x06` UTF-8, `0x07` Bytes, `0x0F` JSON (Listen, Dicts; Bytes als
`{"__bytes__": hex}`). **Ordnung** (auch die des Schreibers): Zahlen (ohne
NaN) nach Wert, dann `str` (Codepunktordnung = UTF-8-Bytes), dann `bytes`,
dann alle übrigen (None, NaN, Container) nach Unterklasse und Kodierung.
Gleiche Zahlen verschiedener Typen (`1`, `1.0`, `True`) sind ein Schlüssel
(wie in einem Python-`dict`).

| Codec | Inhalt der Posting |
|---:|---|
| 4 / 8 | `u`-Werte als u32- bzw. u64-Array (Breite nach dem größten indizierten `u`, mindestens nach dem Lane-Maximum) |
| 0 | VarUInt-Deltas (für `u ≥ 2^64`) |
| 1 | FULLTEXT-Positionen: `FTP1 vu(n) (vu(Δu) vu(tf) vu(pos0) vu(Δpos)…)*` |
| 2 | FULLTEXT-Dokumentlängen: `FTD1 vu(n) vu(total_tokens) (vu(Δu) vu(len))*`, Schlüssel `"\u0000NHYDB_FULLTEXT_DOCUMENT_LENGTHS"` |

Postings sind zlib-komprimiert (Level 1; Unix: System-`libz`, bytegleich zu
CPython; Windows: `miniz_oxide`, gültiger, aber nicht notwendig bytegleicher
Strom). `count` ist die Dokumentfrequenz bzw. Dokumentanzahl;
`posting_count` liest sie aus den Blatteinträgen, ohne Postings zu
dekodieren.

**Lesen.** Beim Öffnen wird nur der Kopf der Wurzel gelesen. Eine Suche
liest je Baumebene eine Seite; jede Seite wird beim Laden per Payload-CRC
geprüft und muss ihrem Elterneintrag entsprechen (Offset, Block-ID, Typ,
Tag, Index, Stufe, Lane), sonst `CorruptionError` – nie stillschweigend
falsche Treffer. Bereichsabfragen durchlaufen die Blätter in
Schlüsselreihenfolge (ohne Kopie der Schlüssel). Dekodierte Seiten liegen in
einem LRU-Cache mit Bytebudget (`[cache] index_page_cache_bytes`, Standard
32 MiB, Abschnitt 9.8). `verify(deep=True)` durchläuft alle Bäume, ohne den
Cache zu füllen (Bindung, Ordnung, Schlüsselzahl, jede Posting dekodierbar
und mit passender Anzahl).

**Schreiben.** `build_indexes` schreibt den Baum als Strom (je Ebene eine
offene Seite; jede Seite nach ihren Kindern, zuletzt die Wurzel; Aufbau und
Speicher: Abschnitt 12.2). Sind seit der
letzten Verzeichniswurzel mindestens 1 024 Seiten (Typ 15) hinzugekommen,
schreibt der Commit eine neue Verzeichniswurzel (auch ohne neue
Datenblöcke), damit das nächste Öffnen diese Seiten nicht scannen muss
(Abschnitt 7.10).

**Format 0.16 (`SI10`).** SECONDARY-Blöcke von NHyDB 0.28.0 (Tag `SI10`)
werden nicht gelesen: Das Tag `SI10` ergibt `CorruptionError` (falsches
Tag); gelesen werden nur `SI28`-Schlüsselbäume.

### 7.9 CHECKPOINT-Block

`CP10` gefolgt von JSON mit `version` (11), `data_max_block_id`, je Realm und
Lane der Segmentanzahl und `max_u` sowie `secondary_source_block`. Ein
Checkpoint wird über `checkpoint()` angehängt und sofort committet. Er macht
frühere Blöcke **nicht** überflüssig; er ist eine Momentaufnahme für
Diagnosezwecke (`health` meldet eine Warnung, wenn keiner existiert).
Seit 0.28.2 schreibt `checkpoint()` außerdem die seit dem letzten
Verzeichnislauf hinzugekommenen Blöcke als Lauf, wenn es sich lohnt
(Abschnitt 7.10).

### 7.10 Blockverzeichnis (Typ 14, ab 0.28.2)

Das Verzeichnis (`db/directory.rs`, `db/segindex.rs`) ersetzt beim Öffnen
großer Datenbanken den Scan aller Blöcke und die vollständige
Segmentverwaltung im Arbeitsspeicher. Es ist **redundant**: Alle Angaben
stehen auch in den Datenblöcken, die es beschreibt. Ein Verzeichnis
beschreibt genau die Blöcke vor seiner Wurzel und wird erst durch einen
Superblock mit `dir_root` gültig.

| Teil | Payload | Inhalt |
|---|---|---|
| Seite | `DP17 u8(kind) vu(realm) u8(lane) vu(n) eintrag*` | höchstens 32 Einträge einer Art (1 = DATA_DENSE, 2 = SPARSE, 3 = DELETE) eines (Realm, Lane), sortiert nach `(u_start, block_id)`, delta-kodiert (`vu(Δu_start)`, `vi(Δblock_id)`, Offsets als `vi(Δ(offset/8))`); DENSE zusätzlich `vu(count) vu(shape) vi(Δoff) vu(payload_len) vu(präfixlänge)`, SPARSE `vu(u_end−u_start) vi(Δoff) vu(payload_len) vu(präfixlänge) vu(groups) vu(cells)`, DELETE nur `vu(Δu) vi(Δblock_id)` |
| Lauf | `DR17 vu(level) vu(teile)` | je Teil `u8(kind) vu(realm) u8(lane) vu(seiten)`, je Seite `vu(offset/8) vu(n) vu(Δerstes_u) vu(max_u_end−erstes_u) vu(max_block_id)` |
| Wurzel | `DT17 vu(len) JSON` | Liste der lebenden Läufe; Offsets des neuesten SYSTEM-, DB_CONFIG- und CHECKPOINT-Blocks; neueste SECONDARY-Blöcke je (Index, Lane); je (Realm, Lane) die Kennzahlen, die sonst der Scan sammelt (Datensatz-/Gruppen-/Zellsummen, Shapes, `max_u`, neueste Daten-Block-ID, Überlagerungsflags) |

**Schreiben.** Ein Lauf der Stufe 0 beschreibt die Blöcke seit dem vorigen
Lauf. Er wird geschrieben, sobald seit dem letzten Lauf
`[directory] flush_blocks` Daten-/DELETE-Blöcke (Standard 65 536) oder
`flush_bytes` Payloadbytes (Standard 256 MiB) hinzugekommen sind – beim
Commit oder nach einem Schreibaufruf –, sowie bei `checkpoint()`, wenn es
sich lohnt (ab 1 024 Einträgen oder 64 MiB und im Mittel ab 8 Einträgen je
Seitenteil). Zusammengeführt wird stufenweise: 8 Läufe einer Stufe ergeben
einen Lauf der nächsten Stufe (O(log n) Neuschreibungen je Eintrag);
abgelöste Verzeichnisblöcke bleiben bis zur Kompaktierung als Müll in der
Datei. Nur das Ziel einer Kompaktierung wird einmal vollständig zu einem
Lauf zusammengeführt. Verzeichnisblöcke liegen vor dem Superblock (ein
Absturz lässt das vorige Verzeichnis gültig) und unterliegen der
DB-Größengrenze (`db_max_size_bytes`). Sind seit der letzten Wurzel
mindestens 1 024 Seiten von Schlüsselbäumen (Typ 15, Abschnitt 7.8)
hinzugekommen, schreibt der Commit eine neue Wurzel (auch ohne neue
Datenblöcke), damit das nächste Öffnen sie nicht scannt.

**Fehlerbehandlung.** Ein Flush nach einem Schreibaufruf ist ein Versuch:
Scheitert er (Größengrenze, E/A-Fehler, beschädigte alte Seite), wird er
zurückgenommen und erst nach einer weiteren Schwelle oder im Commit
wiederholt; der Schreibaufruf selbst meldet keinen Fehler. Kann ein
Verzeichnis im Commit nicht geschrieben werden, wird es zurückgenommen und
der Commit ohne neues Verzeichnis ausgeführt. Scheitert eine
Zusammenführung an einer beschädigten Seite, schreibt das Handle weiter
Läufe der Stufe 0, führt nicht mehr zusammen und meldet den Zustand
(`stats()["directory"]["degraded"]`, Warnung in `verify`); `repair_file`
verwirft das Verzeichnis verlustfrei (`dir_root = 0`). Findet ein Flush
einen beschädigten Block im Tail, wird er zurückgenommen und gemeldet
(`stats()["directory"]["last_flush_error"]`, Warnung in `verify`/`health`).

**Lesen.** Beim Öffnen werden Wurzel und Läufe gelesen (CRC-geprüft),
Seiten erst bei Bedarf (CRC-geprüft, LRU-Cache mit
`[directory] cache_entries` Einträgen). Prüfungen:

* Eine Seite muss ihrer Zusammenfassung im Lauf entsprechen (Anzahl, erstes
  `u`, größtes `u`-Ende, größte Block-ID, Sortierung, Einträge nur auf
  Blöcke davor), auch wenn sie aus dem Cache kommt. Die Wurzel muss jeden
  Lauf und jede Seite genau einmal nennen.
* Ein Datenblock wird beim ersten Laden gegen seinen Eintrag geprüft:
  Payload-CRC, Kopf (Typ, Block-ID, Lane, Länge) und Präfix (Tag, Realm,
  Shape, `u`-Bereich, Zähler, Präfixlänge); ein CRC-gültiger, aber
  vertauschter Block ergibt `CorruptionError` statt der älteren Version.
* Block-IDs steigen mit der Position in der Datei (Öffnen, `verify`,
  `inspect_tail`); ein vertauschter oder kopierter Block wird beim Scan
  erkannt.
* Eine beschädigte Wurzel oder ein beschädigter Lauf führt beim Öffnen zum
  vollständigen Scan (Warnung, kein Fehler); eine beschädigte Seite ergibt
  `CorruptionError` beim Zugriff. Ein Verzeichnisblock mit beschädigtem Kopf
  (Signatur und Typ 14 intakt, Verzeichnis-Tag in der Payload, Payload-CRC
  oder gültiger Folgeblock) wird als redundant übersprungen, statt die Kette
  wie bei beschädigten Daten zu schneiden.
* `verify(deep=True)` vergleicht das ganze Verzeichnis mit den gescannten
  Datenblöcken (Einträge, Kennzahlen und Überlagerungsflags der Wurzel,
  Zugehörigkeit jedes DELETE-Eintrags zu einem DELETE-Block).

Laut `CHANGELOG.md` öffnet eine Datei mit einer Million Einzelzellen-Blöcken
mit Verzeichnis in ≈ 5 ms mit ≈ 7 MB statt ≈ 0,3 s mit ≈ 230 MB.

### 7.11 Speicherbedarf

Gemessene Dateigrößen des Kernbenchmarks (Abschnitt 18.5): 1 Mio.
KQ-Datensätze BASE_ONLY ≈ 23,4 MB (≈ 23,4 Byte je Record bei 11 Feldern), SPARSE_MIX zusätzlich
≈ 9,78 Byte je materialisierter Extension-Zelle. Diese Werte gelten nur für
das jeweilige Benchmark-Schema.

---

## 8. Systemadressraum R0,u1 und externer Layoutplan

### 8.1 Zweck

Das Dateiformat (seit 0.16, unverändert in 0.28.2) trennt **Daten** (in `X.nhydb`) von **Schema und Layout**
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
| v6 | Schema-Digest (HMAC-SHA256 mit Schlüssel, hex) des aktiven Schema-Snapshots | ja |
| v7 | `created_ns` | nein |
| v8 | `"AES-256-GCM + HMAC-SHA256"` | nein |
| v9 | `{"lane_NL":0,"lane_NR":1,"realm_span":10**100}` (nur im ersten SYSTEM-Block) | nein |

v9 steht nur im ersten SYSTEM-Block der Kette; Folgegenerationen enthalten
v9 nicht. `get_system_cell(9)` liefert v9 dennoch aus der gemerkten Kette.

### 8.3 Schlüssel- und Plandatei

* **Schlüsseldatei:** JSON
  `{"contract":"NHYDB027_KEY_V1","db_id","db_name","key_b64"}` mit 32
  Zufallsbytes; exklusiv angelegt (`create_new`), Unix-Rechte `0600`,
  Datei- und Verzeichnis-Sync. Ist die Datei beim Öffnen für Gruppe oder
  andere lesbar (`mode & 0o077`), meldet NHyDB eine `RuntimeWarning` (auch in
  `db.open_warnings`). Der Schlüssel wird im Speicher beim Freigeben genullt
  (Grenzen: Abschnitt 15.6).
  Windows: Die Schlüsseldatei erbt die ACL ihres Verzeichnisses; NHyDB setzt
  keine eigene ACL. Schlüsseldateien daher in ein nur für den Benutzer
  zugängliches Verzeichnis legen (z. B. unter `%USERPROFILE%`) oder über
  `[paths] key_path` an einen geschützten Ort.
* **Plandatei:** JSON `{"contract":"NHYDB027_EXTERNAL_LAYOUT_V1","db_id","entries":[1..2],"mac"}`
  mit `mac = HMAC-SHA256(SHA256("NHYDB027-LAYOUT-MAC:" ‖ key), json(Umschlag ohne mac))`.
* **Eintrag:** `{"generation","sha256","nonce","ciphertext"}`; AES-256-GCM
  mit 12-Byte-Zufallsnonce, AAD `"NHYDB027:{db_id}:{generation}:{sha256}"`,
  Base64-kodiert. `sha256` (Feldname historisch) ist der Schema-Digest
  `HMAC-SHA256(SHA256("NHYDB027-SCHEMA-DIGEST:" ‖ key), Klartext)` – ohne
  Schlüssel lässt sich ein geratenes Schema nicht bestätigen.
* **Klartext:** Schema-Snapshot
  `{"contract":"NHYDB027_SCHEMA_V1","realms","shapes","extensions","layouts","indexes","math_layouts"}`
  (Listen nach ID sortiert).
* JSON überall mit sortierten Schlüsseln, Trennzeichen `,`/`:` und UTF-8 ohne
  ASCII-Escape; MAC und Schema-Digest hängen an dieser bytegenauen Serialisierung.

### 8.4 Öffnen (fail closed)

```mermaid
sequenceDiagram
    participant A as Aufrufer
    participant D as Db::open_locked
    participant F as X.nhydb
    participant K as layout.key
    participant P as layout.enc
    A->>D: NHyDB(path) / Db::open_with
    D->>D: Pfade absolut verankern, Konfiguration auflösen,<br/>Sperre <db>.lock erwerben (nicht bei readonly=True)
    D->>F: beide Superblöcke lesen und bewerten
    D->>D: Commit-Grenze eindeutig? (sonst fail closed, s. u.)
    D->>F: mit dir_root: Wurzel und Läufe lesen (CRC),<br/>neuesten SYSTEM-Block an den ersten binden
    D->>F: EIN Scan bis last_valid_end – nur hinter der Wurzel,<br/>ohne (lesbares) Verzeichnis über alles (Header-CRC)
    Note over D: Datenblöcke registrieren<br/>(Präfixprüfsumme),<br/>SYSTEM-Kette prüfen,<br/>Layout-Metablöcke ablehnen,<br/>Verzeichnisblöcke überspringen
    D->>D: Dateiname == R0,v2 ?
    D->>K: Schlüssel laden (Contract, db_id, db_name, Länge 32)
    D->>P: Plan lesen, HMAC prüfen
    D->>D: genau ein Eintrag mit (Generation, Schema-Digest) aus R0
    D->>P: entschlüsseln (AES-GCM), Schema-Digest und Contract prüfen
    D->>D: Schema importieren, MATH-Zustände validieren,<br/>DATA-Präfixe plausibilisieren
    D->>F: erst jetzt: unkommittierten Tail abschneiden<br/>(nicht bei readonly=True)
    D-->>A: geöffnete Datenbank
```

Jeder Fehler vor dem Abschneiden des Tails lässt die Datei unverändert.
Die Engine prüft R0 und registriert Datenblöcke in **einem** Scan über die
Blockheader. Zeigt der Superblock auf ein Blockverzeichnis (`dir_root`,
Abschnitt 7.10), liest sie dessen Wurzel und Läufe (CRC-geprüft), bindet den
neuesten SYSTEM-Block an den ersten (unveränderliche Zellen gleich,
Generation nicht kleiner), übernimmt DB_CONFIG-, CHECKPOINT- und
SECONDARY-Köpfe aus der Wurzel und scannt nur die Blöcke hinter der Wurzel.
Ohne Verzeichnis oder wenn es nicht lesbar ist (Warnung, kein Fehler) wird
der ganze committete Bereich gescannt. Der Scan prüft außerdem, dass die
Block-IDs mit der Position in der Datei steigen, und bei DATA_DENSE- und
SPARSE_CELLS-Blöcken die Präfixprüfsumme (Abschnitt 7.7; fail closed,
`CorruptionError` „… prefix checksum mismatch“); ganze Payloads von
Datenblöcken liest er nicht. Von SECONDARY-Blöcken liest er nur den Kopf der Wurzel.

**Bewertung der Superblöcke.** Gewählt wird der gültige Superblock mit der
höchsten Generation, dessen Commit-Grenze in der Datei liegt. Bytes hinter
der Grenze gelten nur dann sicher als unkommittiert, wenn **beide**
Superblöcke gültig und konsistent sind. Sonst gilt:

| Lage | Verhalten beim Öffnen |
|---|---|
| beide Superblöcke gültig und konsistent | Tail wird nach allen Prüfungen abgeschnitten (`recovered_tail_bytes`) |
| ein Superblock defekt oder inkonsistent, **keine** Bytes hinter der Grenze des neuesten gültigen Superblocks | öffnet mit `RuntimeWarning` (auch in `db.open_warnings`) |
| ein Superblock defekt oder inkonsistent, Bytes hinter der nutzbaren Grenze (könnten committete Daten sein) | `CorruptionError`, Datei unverändert; `readonly=True` liest den Stand des nutzbaren Superblocks, `repair_file` klärt den Zustand (Abschnitt 15.4) |
| neuester gültiger Superblock zeigt hinter das Dateiende (Datei abgeschnitten) | `CorruptionError` mit Hinweis auf `repair_file`; `readonly=True` liest einen älteren passenden Superblock, falls vorhanden |
| beide Superblöcke defekt bzw. fremde Datei | `CorruptionError` („no usable superblock“ bzw. „not an NHyDB 0.28.2 database file“) |
| Datei kürzer als 8192 Byte | `CorruptionError` („file is shorter than the superblock area …“) |
| Öffnen mit `lock=False` und unkommittiertem Tail | `DatabaseLockedError` (der Tail könnte einem anderen Schreiber gehören; nichts wird abgeschnitten) |

**Wann tritt das auf?** Ein reiner Prozessabbruch (z. B. `SIGKILL`, Absturz
der Anwendung) erzeugt nie einen beschädigten Superblock: Superblöcke werden
mit einem einzigen Schreibaufruf geschrieben, unkommittierte Blöcke werden
beim nächsten Öffnen einfach verworfen. Ein zerrissener Superblock mit
Daten dahinter entsteht nur bei Strom- oder Systemausfall während des
Superblock-Schreibens (oder durch Datenträgerfehler). Dann gilt fail closed:
`readonly=True` liest den Stand des anderen Superblocks;
`repair_file(apply=True, keep_uncommitted=True)` übernimmt die intakten
Blöcke hinter der Grenze (in der Regel der gerade committete Stand),
`allow_data_loss=True` verwirft sie.

**Dateinamensbindung.** R0,v2 speichert den Dateinamen, unter dem die
Datenbank angelegt wurde. Verglichen wird mit dem tatsächlich im Verzeichnis
gespeicherten Namen (Unix: Abgleich über die Inode, Windows: kanonischer
Pfad); auf Dateisystemen ohne Unterscheidung von Groß-/Kleinschreibung zählt
daher die gespeicherte Schreibweise. Eine umbenannte oder unter anderem Namen
kopierte Datei öffnet nicht („must keep the name“); für eine Kopie unter neuem
Namen `compact_file(path, output=neu)` verwenden.

**Lesemodus `readonly=True`.** Die Datei wird mit `O_RDONLY` geöffnet, ohne
Sperre; es wird nichts abgeschnitten und beim Schließen nichts committet.
Schreib-APIs und `commit()` lösen `NHyDBError` aus. Ein unkommittierter Tail
bleibt unsichtbar (`recovered_tail_bytes` meldet seine Länge). Die Sicht ist
der committete Stand zum Zeitpunkt des Öffnens; spätere Commits eines
gleichzeitig geöffneten Schreibers werden nicht nachgeladen (kein
`refresh`). Trifft das Öffnen auf einen gleichzeitig laufenden Schema-Commit
eines Schreibers (Plan und R0 kurz uneinheitlich), wird es wiederholt.
Schreibgeschützte Dateien (z. B. Rechte `0444`) lassen sich so lesen.

### 8.5 Commit mit Schemaänderung (Zwei-Generationen-Protokoll)

```mermaid
sequenceDiagram
    participant D as Db::commit
    participant P as layout.enc
    participant F as X.nhydb
    D->>D: Snapshot exportieren, Generation g+1 versiegeln
    D->>P: Plan atomar ersetzen durch [aktiv(g), neu(g+1)]<br/>(temp. Datei, Sync, atomarer Austausch, Verzeichnis-Sync)
    D->>F: SYSTEM-Block (g+1, neuer Schema-Digest) anhängen
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
closed fehl. Ebenso wird ein wieder eingespielter **älterer** Plan erkannt
(keine Generation passt zu R0 ⇒ `CorruptionError`).

Wird beim nächsten Öffnen nur **ein** Superblock beschädigt vorgefunden und
liegen Bytes hinter der nutzbaren Commit-Grenze, verwirft NHyDB diese Bytes
nicht mehr stillschweigend, sondern verweigert das Öffnen (Abschnitt 8.4).

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
0. ggf. Verzeichnislauf und -wurzel anhängen (Schwellen erreicht, Abschnitt 7.10)
1. Append-Puffer schreiben, Datei synchronisieren (Durability-Modus)
2. Superblock (mit dir_root) mit generation+1 in den anderen Slot schreiben, synchronisieren
```

Ein Absturz vor dem Superblock lässt das vorige Verzeichnis gültig; ein
Verzeichnis, das nicht geschrieben werden kann (z. B. Größengrenze), wird
zurückgenommen und der Commit ohne neues Verzeichnis ausgeführt.

Nach Rückkehr von `commit()` (bzw. einer Operation mit `commit=True`) ist
der Zustand gemäß dem gewählten Durability-Modus dauerhaft. Ohne Commit
geschriebene Daten liegen im Tail und gehen bei einem Absturz verloren.

* Unter macOS ist der Sync in Schritt 1 im Modus `fsync` ein
  `fcntl(F_BARRIERFSYNC)`: Alle zuvor geschriebenen Daten werden vor dem
  Superblock persistiert (Schreibreihenfolge). Unterstützt das Dateisystem
  die Barriere nicht, wird `fsync(2)` verwendet.
* Im Modus `full` ist der Sync in Schritt 1 unter macOS ebenfalls ein
  `F_BARRIERFSYNC` (Reihenfolge), der Sync in Schritt 2 ein `F_FULLFSYNC`:
  Er leert den Laufwerkscache für Daten **und** Superblock, bevor `commit()`
  zurückkehrt. Garantie wie bisher (zwei `F_FULLFSYNC`), aber ≈ 3,6 statt
  6,0 ms je Commit. Ohne Barrierenunterstützung wird in Schritt 1
  `F_FULLFSYNC` verwendet. Absturzanalyse: Ein Ausfall vor dem Superblock
  hinterlässt den alten Superblock (alter konsistenter Stand); ist der neue
  Superblock persistiert, sind es wegen der Barriere auch seine Daten.
* Schlägt ein Sync oder das Schreiben des Superblocks fehl, ist das Handle
  „poisoned“: Weitere Commits werden abgewiesen, die Datenbank muss neu
  geöffnet werden (ein wiederholter Sync könnte Erfolg melden, obwohl Daten
  verloren sind). Ein fehlgeschlagenes Schreiben des Puffers selbst bleibt
  wiederholbar.
* Ein Commit ohne Änderungen schreibt nichts, sobald beide Superblöcke den
  aktuellen Stand tragen; `close()` sorgt dafür, dass beide Slots aktuell
  sind. Öffnen und Schließen ohne Änderungen schreibt nichts, auch nach
  einem Rückfall auf den vollständigen Scan.

### 9.3 Schreibpfade im Überblick

| Operation | Ergebnis im Container | Hinweise |
|---|---|---|
| `put_dense_batch(realm, u_start, rows, …)` | ein `DATA_DENSE`-Block (+ optional ein `SPARSE_CELLS`-Block für Extensions) | Extensions werden **vor** dem ersten Append validiert und kodiert |
| `put(realm, u, values, …)` | wie `put_dense_batch` mit einer Zeile | |
| `put_points(realm, cells, …)` | ein `SPARSE_CELLS`-Block je `u`-Fenster | alle Zellen werden vor dem ersten Append kodiert |
| `put_extensions(…)`, `put_extension_list(…)`, `put_extension_range(…)` | Sparse-Zellen | `merge=False` löscht nicht genannte, lebende Extensions per Tombstone |
| `delete_cell(realm, u, v, …)` | Sparse-Tombstone | schreibt nichts, wenn die Zelle nicht lebt; eine Null-Zelle lebt (Abschnitt 5.2) |
| `delete_area(realm, u_from, u_to, v_from, v_to, …)` | `DELETE`-Blöcke und Sparse-Tombstones | alle lebenden Zellen des Rechtecks, eine Einheit (Abschnitt 5.2) |
| `delete_record(layout_id, object_id, …)` | `DELETE`-Block (CLASSIC) und Sparse-Tombstones | löscht den logischen Record vollständig (Abschnitt 5.2) |
| `put_object(s_batch)` | Zellen gemäß Layout | |
| `put_math_batch(…)` | Sparse-Zellen in exklusiven MATH-Realms | vollständiger Rollback im Fehlerfall (Abschnitt 13) |
| `with db.batch(…) as b:` | ein `put_points` je Realm/Lane beim Verlassen des Blocks | Aufrufe sofort geprüft, letzte gewinnt, kein read-your-writes, bei Ausnahme nichts geschrieben (Abschnitt 16.4) |
| `set_max_db_size(…)` | `DB_CONFIG`-Block | |
| Schemaoperationen | keine Datei-Bytes; `layout_dirty` | neue Plangeneration beim Commit |

```mermaid
flowchart LR
    IN["API-Aufruf<br/>(Python oder Rust)"] --> VAL["Validieren<br/>(Realm, Shape, Typen, Koordinaten,<br/>MATH-Besitz)"]
    VAL --> ENC["vollständig kodieren<br/>(Dense-Rows / Sparse-Stream)"]
    ENC --> LIM{"align8(Blockende)<br/>≤ max_db_size_bytes?"}
    LIM -- nein --> ERR["DatabaseSizeLimitError"]
    LIM -- ja --> APP["Block anhängen<br/>(Header + Payload + Padding)"]
    APP --> REG["Segment registrieren<br/>(Verzeichnis-Tail, Intervallindex, Caches;<br/>ggf. Verzeichnis-Flush, Abschnitt 7.10)"]
    REG --> C{"commit=True?"}
    C -- ja --> COMMIT["commit()"]
    C -- nein --> TAIL["bleibt im Tail"]
```

**Atomarität von Mehrblock-Operationen (seit 0.28.2):** Eine Operation, die
mehrere Blöcke schreibt, ist eine Einheit: `put_points` über mehrere
`u`-Fenster, `put_dense_batch` mit `extensions_by_u`,
`put_extensions(merge=False)`, `put_objects_batch`, `put_objects_numpy`,
`delete_record`, das Schreiben eines `batch()`, `put_math_batch` und jeder
SECONDARY-Baum von `build_indexes`. Schlägt ein späterer Append fehl (z. B.
Größengrenze, I/O), schneidet die Schreibklammer (`Db::write_scope`) die
Datei auf die Länge vor der Operation zurück und baut den Zustand im Speicher
neu auf; ein späterer Commit kann keine Teilreste übernehmen. Frühere,
unkommittierte Schreibvorgänge des Objekts bleiben erhalten. Scheitert der
Rückbau selbst, ist das Objekt gesperrt (neu öffnen). Bäume, die
`build_indexes` vor dem Fehler fertig geschrieben hat, bleiben gültig.

### 9.4 Lesepfade

* **Dense:** `resolve_segment(realm, u, lane)` sucht alle Segmente, die `u`
  enthalten – in den Verzeichnisseiten (Abschnitt 7.10, bei Bedarf geladen)
  und im Intervallindex der Blöcke seit dem letzten Verzeichnislauf
  (`interval.rs`) –, und wählt das mit der höchsten Block-ID, sofern kein
  jüngerer `DELETE`-Block existiert. Das Recordbyte-Fenster ergibt sich aus
  der Offsettabelle. Der Intervallindex hält seine Einträge seit 0.28.2 in
  sortierten Teilblöcken zu höchstens 512 Einträgen (statt eines sortierten
  Vektors); Einfügen außerhalb der Reihenfolge kostet damit nicht mehr O(n),
  der Aufbau ist für jede Schreibreihenfolge O(n log n).
* **Sparse:** Für gezielte Sparse-Zugriffe je `u` (u. a. COLUMN/PACKED) baut die
  Engine je `(realm, lane, u)` einmalig einen **v-Index** (`vindex.rs`) aus
  zwei Teilen:
  * **Einzelzellen-Records** (eine Zelle von `u` in einem Block, typisch für
    Einzelschreibvorgänge): eine sortierte Abbildung `v → neuester Record`.
    Ein neuerer Record derselben Zelle ersetzt den älteren; überschriebene
    Versionen kosten weder Speicher noch Suchzeit (eine Million Versionen
    einer Zelle: Index ≈ 0 MB statt ≈ 300 MB, Punktlesen ≈ 0,3 µs statt
    ≈ 3 ms laut `CHANGELOG.md`).
  * **Mehrzellen-Records:** ihr v-Bereich im Intervallindex und – nur bei
    mehr als 128 Zellen – Sprungmarken alle 128 Zellen.

  Ein Punktzugriff nimmt den Einzelzellen-Kandidaten und die Mehrzellen-Records,
  deren Bereich `v` enthält, vom neuesten zum ältesten Block, springt über
  eine Marke nahe `v` und dekodiert nur die Trefferzelle. Der Aufbau ist
  O(n log n) für jede Schreibreihenfolge; bis zu 64 Geschwister-u-Gruppen
  mit derselben Blockmenge werden gemeinsam indexiert. Budget:
  `[cache] vindex_max_marks` (Bytebudget in Einheiten einer Sprungmarke zu
  48 Byte, Standard 4 000 000 ≈ 192 MB) und `vindex_max_indexes` (4 096),
  Verdrängung nach LRU; ein einzelner Index über dem Budget wird allein
  gehalten. Richtwerte: ≈ 60 Byte je lebendem Einzelzellen-Record, ≈ 140 Byte
  je Mehrzellen-Record plus 48 Byte je 128 Zellen.
* **Overlays:** `get_internal` kombiniert Dense-Record und neueste
  Sparse-Zustände derselben u-Gruppe. Hat die Realm/Lane keine
  Sparse-Overlays, dekodieren `get_value`/`get_cell` nur das angefragte
  BASE-Feld (die vorherigen Felder werden übersprungen).
* **Sparse-Schreiben und v-Index:** Ein neuer Sparse-Block aktualisiert die
  gecachten v-Indizes nicht sofort; ein v-Index zieht die neuen Blöcke beim
  nächsten Lesen seines `u` in Blockreihenfolge nach (fortgeschrieben, nicht
  neu aufgebaut). Seit 0.28.2 gilt das auch über einen Verzeichnis-Flush
  hinweg: Die gecachten Indizes werden nicht mehr verworfen, und
  Verzeichnisseiten ohne neuere Blöcke werden nicht gelesen. Dekodierte
  Sparse-Records (Schlüssel Block-ID und `u`, unveränderlich) bleiben
  gültig.
* **Bereiche:** `range`/`select` finden das nächste gespeicherte `u` über
  den Intervallindex in O(log Segmente) und überspringen Lücken.
* **Block-Cache:** Budget `[cache] block_cache_bytes` (Standard 64 MiB),
  Blöcke bis max(8 MiB, Budget/8), Verdrängung nach LRU. Jeder Block wird
  vor der ersten Nutzung vollständig gelesen und per Payload-CRC geprüft
  und danach als *geprüft* vermerkt; nicht (mehr) gecachte geprüfte Blöcke
  werden nur noch bereichsweise gelesen (Offsettabelleneintrag + Record bzw.
  Sparse-Fenster zwischen zwei Sprungmarken). Große Batches bleiben damit
  auch für Einzelzugriffe schnell. Der Vermerk gilt je Handle und wird bei
  jedem Zurückschneiden der Datei (Rollback, verworfener Puffer) und beim
  Schließen verworfen; `verify` prüft immer vollständig. Details und alle
  Cache-Budgets: Abschnitt 9.8.

### 9.5 Iteratoren

`range`, `select`, `select_us`, `find`, `iter_materialized_sparse_points`,
`iter_materialized_cells` und `iter_materialized_record_ns` liefern in
Python **lazy Iteratoren** (`ResultIterator`): Ergebnisse werden blockweise
nachgeladen, Fehler treten erst beim ersten `next()` auf, der
Speicherbedarf ist unabhängig von der Ergebnisgröße.

* Nach `close()` löst jedes weitere `next()` `NHyDBError("database is closed")` aus, auch wenn noch Zeilen
  gepuffert sind.
* `find` ermittelt die Liste der passenden `u` beim ersten `next()` aus dem
  Index (dabei wird die Frische geprüft, `StaleIndexError`). Danach arbeitet
  der Iterator mit dieser Momentaufnahme: Spätere Schreibvorgänge lösen keinen
  `StaleIndexError` aus und werden nicht berücksichtigt. Für einen aktuellen
  Stand einen neuen Iterator erzeugen.
* Verwirft dasselbe Datenbankobjekt Daten – `rollback()` mit ausstehenden
  Änderungen oder der nach Erkennen eines fremden Schreibers verworfene
  Append-Puffer (`lock=False`) –, löst der nächste Schritt jedes Iterators
  `NHyDBError` („iterator invalidated“) aus; die Abfrage ist neu zu starten.
  Schreiben und Commits zwischen zwei Schritten bleiben erlaubt; ein
  `rollback()` ohne ausstehende Änderungen und `readonly`-Handles sind nicht
  betroffen.
* `iter_materialized_sparse_points` verwendet seit 0.28.2 dieselbe
  speicherbegrenzte Zusammenführung wie die Kompaktierung (Abschnitt 15.5;
  Speicher unabhängig von der Zahl der Blöcke). Temporäre Läufe einer
  externen Zusammenführung liegen dabei im System-Temp-Verzeichnis oder –
  falls dieses nicht nutzbar ist – neben der Datenbank.

### 9.6 Durability-Modi

| Modus | macOS | Linux | Windows |
|---|---|---|---|
| `fsync` (Standard) | `fsync(2)` | `fsync(2)` | `FlushFileBuffers` |
| `full` | `fcntl(F_BARRIERFSYNC)` vor, `fcntl(F_FULLFSYNC)` nach dem Superblock | `fsync(2)` | `FlushFileBuffers` |

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
  (`sync_dir`) in beiden Modi wirkungslos: Verzeichniseinträge sind
  journalisierte NTFS-Metadaten der benannten Datei, und der Sync einer neu
  angelegten Datei schreibt das Journal bis zu ihrer letzten Änderung fest.
  Der atomare Austausch (`atomic_replace`) verwendet dort `std::fs::rename`
  (POSIX-Semantik: ersetzt ein vorhandenes Ziel auch, wenn ein
  Readonly-Handle es noch offen hält; lange Pfade werden unterstützt) und
  synchronisiert danach die ersetzte Datei im gewählten Modus, womit die
  Umbenennung dauerhaft ist. Hält ein anderer Prozess (Virenscanner,
  Indexdienst) die Datei kurz, wird bis zu achtmal mit kurzer Wartezeit
  wiederholt (Fehler 5/32). Unter Unix `rename` mit anschließendem
  Verzeichnis-Sync.
* Vor dem Superblock-Schreiben verwendet der Modus `fsync` unter macOS
  `F_BARRIERFSYNC` (Abschnitt 9.2). Damit ist die Commit-Grenze auch im
  Modus `fsync` gegen Stromausfall geschützt: Der Superblock kann nie vor den
  Daten, auf die er verweist, auf dem Datenträger landen. Gemessen kostet das
  rund 0,3 ms je Commit (0,063 → 0,365 ms); `full` leert zusätzlich den
  Laufwerkscache – mit einem einzigen `F_FULLFSYNC` nach dem Superblock
  (Abschnitt 9.2; Commit `full` mit 100 Records ≈ 6,0 → 3,6 ms).
* Schreibweisen: `durability` akzeptiert neben `"fsync"` die Aliasse
  `"default"` und `"normal"`, neben `"full"` den Alias `"fullfsync"`;
  `db.durability` liefert immer den kanonischen Namen.

### 9.7 Lebenszyklus

| Methode | Wirkung |
|---|---|
| `NHyDB.create(path, overwrite=False, …)` | legt Triade an (Schlüssel, Plan mit Generation 0, DB mit SYSTEM-Block), setzt die Größengrenze (`DB_CONFIG`) und committet |
| `NHyDB(path, …)` / `nhydb.open` | öffnet fail closed (Abschnitt 8.4); `readonly=True` öffnet ohne Sperre und ohne die Datei zu ändern |
| `commit()` | Commit-Protokoll; Rückgabe: Commit-Grenze |
| `rollback()` | verwirft alle Schreibvorgänge und Schemaänderungen des Objekts seit seinem letzten Commit: Datei auf die Commit-Grenze zurückschneiden, Caches leeren, Zustand per Open-Scan und Schema aus dem aktiven Layoutplan-Eintrag neu aufbauen (Daten, R0,u1, MATH-Zustände, Größengrenze). Sitzungseinstellungen (Plugins, `durability`, dyn-Ausgabe) bleiben. Ohne Änderungen wirkungslos. `readonly=True` oder „poisoned“ ⇒ `NHyDBError`; mit `lock=False` vorher Prüfung auf fremde Commits (`DatabaseLockedError`, nichts wird abgeschnitten). Rückgabe: Commit-Grenze |
| `sync(hard=False)` | Puffer schreiben, optional Datei-Sync – **kein** Superblock-Commit |
| `close(commit=True)` | optional Commit, danach Dateihandle und Sperre freigeben (sofort, nicht erst bei der Objekt-Freigabe); anschließend lösen alle Lese-, Schreib-, Index- und Abfrage-APIs sowie offene Iteratoren `NHyDBError` ("database is closed") aus, unabhängig von Cache-Inhalten; wiederholte Aufrufe sind wirkungslos; schlägt der Commit fehl, bleibt die Datenbank geöffnet (erneuter Versuch oder `close(commit=False)`), außer das Handle ist „poisoned“ (Abschnitt 9.2) – dann werden Handle und Sperre freigegeben und der Fehler gemeldet; `close(commit=False)` gibt Handle und Sperre auch dann frei, wenn das Schreiben des Puffers fehlschlägt; bei `readonly=True` wird nie committet |
| `with nhydb.create(...) as db:` | `__exit__` ruft `close(commit=True)` auf – **auch wenn im Block eine Ausnahme auftrat** (Semantik der Referenz v0.14). Schlägt dieser Commit fehl, wird die Datenbank mit `close(commit=False)` geschlossen (Handle und Sperre frei) und der Commit-Fehler ausgelöst, verkettet mit der ursprünglichen Ausnahme (`__cause__`); `with` auf einem geschlossenen Objekt löst `NHyDBError` aus |
| Objekt-Freigabe ohne `close` | Puffer wird geschrieben, aber nicht committet |

### 9.8 Caches und Speicherbudgets

Alle Caches gehören zu einem geöffneten Objekt (Handle); zusammen gilt für
alle Objekte eines Prozesses das Prozessbudget `[cache] total_bytes`
(Abschnitt „Prozessbudget“ unten). Die Caches werden beim
Schließen, bei `rollback()` und bei jedem Zurückschneiden der Datei
geleert bzw. ab der Schnittstelle verworfen. Kein Cache ersetzt eine
Prüfung: Daten werden erst nach erfolgreicher CRC-Prüfung ihres Blocks
benutzt.

**Annahme „Blöcke sind unveränderlich“:** Die Prüfung gilt je Handle
und je Block. Ein bereits geprüfter Block wird danach aus dem Cache oder per
Teil-Lesen ohne erneute CRC benutzt, weil NHyDB Blöcke nach dem Anhängen nie
ändert. Ändert ein anderer Prozess die Datei *an Ort und Stelle* (Manipulation,
Werkzeuge außerhalb von NHyDB, oder `repair_file(allow_data_loss=True)`, das
die Datei kürzt, gefolgt von neuen Schreibvorgängen), erkennt ein bereits
offenes Handle – typischerweise ein `readonly=True`-Leser ohne Sperre – das
nicht: Es liefert gecachte alte Werte oder liest neue Bytes ab einem früher
geprüften Offset ohne CRC. Ein neu geöffnetes Handle prüft wieder. Nach einer
solchen Änderung sind offene Handles daher zu schließen und neu zu öffnen.
Kompaktierung und `restore_backup` ersetzen die Datei dagegen atomar; offene
Handles lesen dann unverändert die alte Datei weiter.

| Cache | Budget | Verhalten |
|---|---|---|
| Block-Cache (Payloads) | `[cache] block_cache_bytes`, Standard 64 MiB; `0` = aus | LRU; Verdrängung in Schüben auf ¾ des Budgets; größter Eintrag max(8 MiB, Budget/8), höchstens das Budget (64 MiB ⇒ 8 MiB, 256 MiB ⇒ 32 MiB) |
| „verified“-Register | kompakte Menge der Blockpositionen, wenige Bit je geprüftem Block, ohne feste Obergrenze der Blockzahl | erlaubt Teil-Lesen (`pread` von Offsettabelleneintrag + Record bzw. Sparse-Fenster); Payloadlänge und Teil-Lese-Zähler nur für kürzlich geprüfte Blöcke; nach 8 Teil-Lesungen wird ein cachebarer Block wieder ganz geladen (erneute CRC) und gecacht. Wird ein so zurückgeholter Block verdrängt, bevor er sich gelohnt hat (weniger Treffer als etwa einer je 16 KiB Block, mindestens 64), steigt diese Schwelle (×4, bis 8 · 2^16); lohnt er sich deutlich (4×), sinkt sie (÷2). Das verhindert ein Aufschaukeln bei Zufallszugriffen über mehr Blöcke, als in den Cache passen. |
| Blockverzeichnis-Seiten | `[directory] cache_entries`, Standard 262 144 Einträge (mindestens 128, ≈ 130 Byte je Eintrag) | LRU über dekodierte Seiten; jede Seite wird auch aus dem Cache gegen ihre Zusammenfassung im Lauf geprüft (Abschnitt 7.10) |
| Index-Seiten (Schlüsselbäume, Abschnitt 7.8) | `[cache] index_page_cache_bytes`, Standard 32 MiB (geschätzte Bytes der dekodierten Seiten) | LRU über dekodierte Blätter und innere Seiten aller Indizes; eine Suche liest je Baumebene eine Seite; `verify(deep=True)` füllt den Cache nicht |
| Index-Postings, FULLTEXT-Termpostings, FULLTEXT-Dokumentlängen-Segmente | `[cache] posting_cache_bytes`, Standard 256 MiB, gemeinsam (geschätzte Bytes, große `Nat` und Werte mitgezählt) | LRU; ein Eintrag über dem Budget wird nicht gecacht; Bereichsabfragen über mehr als 64 Schlüssel dekodieren ihre Postings, ohne sie zu cachen |
| Sparse-Gruppenverzeichnisse, dekodierte Sparse-Records, zusammengeführte u-Gruppen | `[cache] sparse_cache_bytes`, Standard 128 MiB, gemeinsam | LRU; Schlüssel Blockoffset, (Block-ID, `u`) bzw. (Realm, Lane, `u`) |
| Feldtypen je Adresse (`field_meta`) | Teil des Prozessbudgets; geleert ab 65 536 Einträgen | wird unter Druck als eigener Pool ganz geleert |
| v-Index (gezielter Sparse-Zugriff je `u`) | `[cache] vindex_max_marks` (Einheiten zu 48 Byte, Standard 4 000 000 ≈ 192 MB) und `vindex_max_indexes` (4 096) | LRU; ein einzelner Index über dem Budget wird allein gehalten; ≈ 60 Byte je lebendem Einzelzellen-Record, ≈ 140 Byte je Mehrzellen-Record plus 48 Byte je 128 Zellen; nach neuen Blöcken (auch über einen Verzeichnis-Flush) fortgeschrieben statt neu aufgebaut (Abschnitt 9.4) |
| FULLTEXT-Dokumentlängen | seitenweise im Schlüsselbaum (ab 0.28.2), über Seiten- und Posting-Cache | eine Rangsuche liest nur die Segmente ihrer Treffer; kein Korpus je Dokument im Speicher (Abschnitt 12.4) |
| Zusammenführung der Sparse-Blöcke (Kompaktierung, `iter_materialized_sparse_points`) | Lesefenster aller offenen Blockströme zusammen 32 MiB (je Strom 4 KiB bis 1 MiB); höchstens 4 096 gleichzeitig offene Blöcke | Werte über 256 Byte werden erst gelesen, wenn ihre Zelle gewinnt; bei mehr als 4 096 an einem `u` überlappenden Blöcken externe Zusammenführung über Läufe in **einer** anonymen temporären Datei (Abschnitt 15.5) |

**Nicht budgetierte Strukturen der Indizes (strukturelle Grenzen).** Der
Speicher der Indizes ist **nicht vollständig** begrenzt. Ohne Budget
bleiben:

* **Aufbau** (`build_indexes`, Abschnitt 12.2): Deduplikationsmengen
  proportional zur Zahl der Records einer Realm/Lane, wenn sich
  Dense-Segmente überlappen oder Deletes existieren; die Abbildung der
  Sparse-Overlays indizierter BASE-Felder; die Menge der Sparse-Zellen
  indizierter Extension-Felder; die Posting-Liste des gerade geschriebenen
  Schlüssels (ein Schlüssel mit sehr vielen Dokumenten ≈ 8 Byte je Dokument;
  der Dokumentlängen-Schlüssel eines FULLTEXT-Index hält einen Eintrag je
  Dokument).
* **Aufbau, Prüfpunkte:** Das Budget wird alle 256 Dense-Datensätze bzw. 64 KiB
  Datensatzbytes und alle 256 Gruppen eines SPARSE-Blocks geprüft; der
  gerade gelesene Block selbst liegt ganz im Speicher. Deduplikationsmengen
  entstehen nur noch für sich überschneidende Blöcke.
* **Abfragen:** Ergebnis- und Vereinigungsvektoren proportional zur
  Ergebnisgröße; große Ergebnisse lassen sich abschnittsweise lesen oder in
  Ergebnisdateien schreiben (Abschnitt 14.5).
* Die Diagnose-API `_ensure_secondary_directory` (Rust:
  `secondary_directory()`) liefert **alle** Schlüssel eines Index.

Ist ein Budget erreicht, wird der jeweilige Cache (außer Block-Cache,
v-Index, Index- und Verzeichnisseiten, die nach LRU verdrängen) geleert, nachdem der
tatsächliche Verbrauch neu berechnet wurde. Der Block-Cache rechnet je
gecachtem Block 112 Byte Verwaltungsaufwand auf sein Budget an.
`stats()["block_cache"]` liefert `budget_bytes`, `max_item_bytes`,
`cached_bytes`, `cached_blocks`, `verified_blocks` und die Zähler `hits`,
`loads`, `partial_reads`, `evictions` (seit dem Öffnen), seit 0.28.2 dazu
für den v-Index-Cache `vindex_indexes`, `vindex_bytes`,
`vindex_budget_bytes`, `vindex_builds` und `vindex_catchups` sowie für den
Seiten-Cache der Indizes `index_pages`, `index_page_bytes`,
`index_page_budget_bytes`, `index_page_reads` und `index_page_evictions`.
`stats()["directory"]` liefert `root_offset`, `runs`, `levels`,
`persisted_entries`, `live_pages`, `tail_entries`, `tail_bytes`, `flushes`,
`merges`, `pages_written`, `bytes_written`, `loaded`, `fallback_full_scan`,
`failed_flushes`, `damaged_blocks_skipped`, `degraded` (Grund, wenn das
Verzeichnis wegen einer beschädigten Seite nicht mehr zusammengeführt wird),
`last_flush_error` (ein Flush wurde wegen eines beschädigten Blocks im Tail
abgebrochen; auch als Warnung in `verify`/`health`), `cached_pages` und
`cached_entries`.

**Prozessbudget** (`[cache] total_bytes`, Standard 4 GiB, `0` = alles
sofort verdrängen):
- **Buchung:** Jede Änderung eines Caches wird beim Freigeben seiner Sperre im Prozess-Topf gebucht.
- **Verdrängung:** Über dem Budget wird bei allen gerade nicht beschäftigten Objekten verdrängt, bis 7/8 erreicht sind, nur über `try_lock`, also ohne Blockieren. Ein Objekt, dessen Cache-Sperre nach einer Panik vergiftet ist, wird weiter verdrängt.
- **Reihenfolge:** Innerhalb eines Objekts zuerst der größte Pool. Pools, die nichts freigeben, werden übersprungen. Das Verifikationsregister kommt zuletzt, denn ihn zu leeren kostet erneute CRC-Prüfungen.
- **Zähler:** Der Bytezähler des Registers ist exakt; im Debug-Build wird er gegen die Summe geprüft.
- **Kurzzeitige Überschreitung:** um gerade eingefügte Einträge beschäftigter Objekte und um Caches eines Objekts während seines Öffnens.
- **Festlegung:** Das erste Objekt, das geöffnet wird, während kein anderes offen ist, legt das Budget fest. Es gilt, solange irgendein Objekt offen ist; abweichende Werte geben eine Warnung.

**Leistungswirkung (Messung Abschnitt 18.5):** Einzelzugriff in einen
Block mit 1 Mio Records nach der ersten Prüfung `get` 4,2 ms → 3 µs;
4 000 zufällige Zugriffe über 400 Blöcke (≈ 100 MB) warm 21 µs → 3,6 µs
(Budget 32 MiB), 2,2 µs (64 MiB), 0,26 µs (256 MiB). Der erste Zugriff auf
einen Block kostet unverändert das einmalige Lesen und Prüfen des ganzen
Blocks (1 Mio KQ-Records ≈ 25 MB, wenige ms). Sekundärindizes als
Schlüsselbäume (gemessen, Abschnitt 18.5): erste Punktabfrage 0,14–0,2 ms
statt 200 ms, RSS nach ≈ 15 000 gemischten Abfragen 75–90 MB statt 722 MB.

### 9.9 Zentraler RAM-Limiter

Jeder Prozess hat **genau einen** Limiter. Er beobachtet den Speicher des
eigenen Prozesses und bremst bei Speicherdruck die großen Vorgänge. Er
verwaltet keine Budgets und lässt keine Vorgänge zu; Caches, Spill-Verfahren
und `max_result_bytes` bleiben unverändert.

**Wasserstand und Grenze.** Gemessen wird der Footprint des ganzen Prozesses
(einschließlich der Python-Anwendung):

| Plattform | Quelle |
|---|---|
| macOS | `phys_footprint` (`task_info`) |
| Linux | Resident Set (`/proc/self/statm`); die RAM-Seite ist zusätzlich durch die cgroup-v2-Grenze `memory.max` begrenzt. `memory.current` wird bewusst nicht verwendet, weil es den Seitencache mitzählt |
| Windows | `PrivateUsage` (Commit) |

`Grenze = (100 − reserve_percent) % × RAM`; die gelbe Zone beginnt
`warn_percent` % des RAM darunter. `ram_bytes` setzt einen kleineren RAM
für den Limiter (z. B. um NHyDB nur einen Teil einer großen Maschine zu
geben).

**Überwachungsthread.** Er startet erst mit dem ersten großen Vorgang,
misst dann alle `tick_ms` (Standard 20 ms) und schläft ohne Vorgänge und im
grünen Bereich ganz (keine Weckvorgänge). Kosten während aktiver Vorgänge
≈ 0,1–0,25 % eines Kerns. Ein Fehler im Thread hebt alle Wartezustände auf;
der Limiter beobachtet dann nur noch (`error` im Status). Nach `fork()`
startet das Kind beim ersten großen Vorgang einen eigenen Thread mit
zurückgesetzten Zählern.

**Vorgänge und Kontrollpunkte.** Große Vorgänge melden sich an, sobald sie
1 MiB gewachsen sind, und prüfen ihr Signal an ihren Stückgrenzen (etwa je
MiB): sammelnde Abfragen (`sql()`, `execute()`, `search()`, `sql_numpy()`,
`ResultFile.fetchall()/to_numpy()`), die Python-Umwandlung großer Ergebnisse,
Sortierungen streamender Abfragen, große Kandidatenmengen (auch NOT/OR),
FULLTEXT-Postings, die TRIGRAM-Nachprüfung, die Spaltenprojektion von
`sql()` (`collect`), `build_indexes`,
Schreibaufrufe (zwischen zwei Blöcken) und `compact_file`. Einzeloperationen
melden sich nie an. Ein Kontrollpunkt kostet im Normalfall ≈ 2,5 ns.
Je Thread gibt es höchstens einen Vorgang: Ein Teilschritt, der in einem
Thread mit bereits angemeldetem Vorgang wächst (etwa das Schreiben der
Indexbäume innerhalb von `build_indexes`), meldet sich nicht zusätzlich an,
sondern bucht sein Wachstum auf diesen Vorgang (Zähler `jobs_joined`).

**Entscheidungen.**

| Zone | Wirkung |
|---|---|
| grün | nichts; wartende Vorgänge laufen weiter |
| gelb | Caches werden bis zur gelben Schwelle geleert (mit Rückmeldung: hat das Leeren den Footprint nicht gesenkt, z. B. wegen des macOS-Allokators, wird die Entlastung beendet und die Caches füllen sich wieder); elastische Vorgänge (Sortierung, Indexaufbau) lagern vorzeitig aus (`SHRINK`); neu begonnene Vorgänge warten |
| rot | alle Vorgänge bis auf den ältesten warten |
| immer | nie warten alle Vorgänge gleichzeitig: der älteste läuft |

**Letzter Ausweg (`cancel_over_limit`, Standard an).** Liegt der Pegel
5 Takte lang über der Grenze, obwohl alle übrigen Vorgänge warten, und
wächst der einzig laufende weiter, wird er abgebrochen:
`ResourceLimitError` mit `reason = "cancelled_by_limiter"`. Ein Schreibaufruf
wird dabei vollständig zurückgerollt. Danach läuft der nächste Vorgang.
Ein weiterer Abbruch setzt echten Mehrverbrauch voraus: Das 5-Takt-Fenster
zählt erst wieder, wenn der Pegel mehr als 64 MiB über dem Pegel des
letzten Abbruchs liegt (oder zwischendurch unter die Grenze gefallen ist).
So bleibt Speicher, den ein abgebrochener Vorgang erst nach und nach
zurückgibt (Windows-Heap: bis ≈ 20 s), ohne Folgeabbrüche (Zähler
`cancel_held_ticks`). Unter Linux (glibc) gibt ein abgebrochener Vorgang
beim Abmelden den freigegebenen Speicher mit `malloc_trim(0)` an das System
zurück – in seinem eigenen Thread, nach dem Abbau seiner Daten, außerhalb
jeder Limiter-Sperre (Zähler `trims`, `trim_ms_total`); glibc behielt ihn
sonst im Prozess.
Mit `cancel_over_limit = false` wird die Lage nur gezählt.

**`TEST_IT`.** Bei hohem, stabilem Pegel darf ein wartender Vorgang um
`probe_bytes` (256 MiB) wachsen. Steigt der Footprint dabei deutlich weniger
als gemeldet (der Prozess nutzt bereits gehaltenen, freien Speicher, etwa den
Large Cache unter macOS), bekommt er die nächste Probe; sonst wartet er
wieder.

**Python und GIL.** Ein Vorgang wartet nie mit gehaltenem GIL; die Bindung
holt ein solches Warten nach, sobald der GIL frei ist. Ebenso wartet kein
Vorgang mit einer gehaltenen internen Sperre eines Handles: die
Overlay-Berechnung eines veralteten Index läuft ohne die Overlay-Sperre, die
nur zum Nachsehen und Ablegen genommen wird. Löschen
(`delete_area`, `delete_record`) und das Schreiben eines `batch()` nehmen
mit Prüfpunkten am Limiter teil.

**Anwendung.**

* `nhydb.resource_status()`: Zone, Pegel, Grenzen, Quelle, Modus
  (`active`, `dry_run`, `observe`), Vorgänge (`id`, `path`, `kind`, `state`,
  `waiting_s`, `waiting_long` …), Zähler je Ereignis und je Pfad, Hinweise.
* `nhydb.cancel_job(id)`: bricht einen Vorgang am nächsten Kontrollpunkt ab,
  auch einen wartenden (`reason = "cancelled_by_app"`). `True` heißt: der
  Abbruch ist angefordert. Hat der Vorgang danach keinen Kontrollpunkt mehr
  (z. B. das Ende eines Merge-Rebuilds der Indexpflege), läuft er zu Ende.
* Wartezeiten führen standardmäßig zu keinem Abbruch; ab `notify_wait_s`
  ist ein Vorgang als `waiting_long` markiert. `wait_timeout_s > 0` bricht
  nach dieser Wartezeit ab (`reason = "wait_timeout"`).

**Betriebsstatistik `limiter.txt` (`[limiter_log]`).** Der Limiter führt
seine Zähler laufend im Speicher. Alle `interval_min` Minuten (Standard 30,
erlaubt 1 … 527 040 = ein Jahr, sonst `ValueError`)
übergibt der Überwachungsthread einen abgeschlossenen Datensatz mit
UTC-Zeitstempel an einen eigenen Hilfsthread; der Limiter wartet dabei nie
(begrenzter Puffer `buffer_records`, volle Puffer werden gezählt). Der
Hilfsthread hängt den Datensatz als eine JSON-Zeile an `limiter.txt` an und
prüft erst danach die Größe: ab `max_bytes` (Standard 60 MB, Rotationsschwelle,
keine harte Grenze) wird die Datei als `limiter_<UTC-Zeit>.zip` archiviert
und neu begonnen. Schreibfehler erscheinen nur in
`resource_status()["log"]["last_error"]`. Beim Schließen des letzten Handles
des Prozesses wird der angefangene Abschnitt ebenfalls geschrieben
(`"final": true`).

* Ort: `path` (Verzeichnis oder Datei `*.txt`); Standard ist das Verzeichnis
  der Datenbank, deren Handle den Limiter festgelegt hat. Der Limiter gilt je
  Prozess: Jede Zeile nennt `pid` und diese Datenbank (`db`); Prozesse, die in
  dasselbe Verzeichnis schreiben, teilen sich die Datei (jede Zeile ist ein
  einzelner anhängender Schreibvorgang).
* Felder einer Zeile: `ts` (Ende des Abschnitts), `from` (Beginn),
  `interval_s`, `final`, `pid`, `db`, `mode`, `source`, `ram_bytes`,
  `limit_bytes`, `yellow_bytes`, `level_bytes` und `zone` (Zustand am Ende),
  `jobs_now`, `log_dropped` (vom Puffer verworfene Datensätze, Summe seit
  Prozessstart), `counters` und `paths` (registrierte Vorgänge und
  Kontrollpunkte je Pfad).
* `counters` sind Werte **des Abschnitts**: Ereigniszähler als Differenz
  zum vorigen Datensatz (jedes Ereignis zählt in genau einem Datensatz),
  Spitzenwerte (`peak_level`, `peak_over_limit`, `max_rise_per_tick` in
  Bytes, `wait_ms_max` in ms) als Höchstwert des Abschnitts. Zeiten in
  ms (`green_ms`, `yellow_ms`, `red_ms`, `wait_ms_total`), sonst Anzahlen:
  `ticks`, `zone_changes`, `signals_wait/resume/shrink/cancel`,
  `waits_deferred` (Warten wegen gehaltenem GIL nachgeholt), `long_waits`,
  `wait_timeouts`, `jobs_registered`, `jobs_finished`, `jobs_joined`,
  `trims`, `trim_ms_total` (ms), `probes`,
  `probes_reused/grew/invalid`, `cache_relief_requests`,
  `cache_relief_bytes` (Bytes), `cache_relief_ineffective`,
  `over_limit_single_ticks`, `cancel_held_ticks`, `dry_*`, `sample_errors`, `thread_starts`,
  `tick_panics`.

**Grenzen.** Der Limiter schützt nach bestem Bemühen; er verspricht keine
Grenze. Er sieht den ganzen Prozess, steuert aber nur NHyDB-Vorgänge;
Speicher der Anwendung und vom Allokator gehaltener freier Speicher zählen
mit. Wartende starre Vorgänge (sammelnde Abfragen) behalten ihren Speicher.
Andere Prozesse auf derselben Maschine schützt er nicht. Für Dienste unter
macOS empfiehlt sich `MallocLargeCache=0` beim Start, damit freigegebener
Speicher sofort an das System zurückgeht.

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
* `occupied_addresses(db, layout, object_id, lane)` wird auch während
  `delete_record` aufgerufen; dort ist `db` gesperrt (`RuntimeError: Already
  mutably borrowed`). Plugins sollten die Adressen daher nur aus `layout`,
  `object_id` und `lane` berechnen und `db` in dieser Methode nicht benutzen.

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
* **Lange Werte in VALUE-Indizes:** Ein VALUE-Index speichert den
  ganzen Feldwert als Schlüssel. Werte bis etwa 1 000 Zeichen sind
  unauffällig. Ab etwa 2 000 Zeichen passen nur noch zwei Einträge in eine
  innere Seite; der Baum wird binär und größer als die Daten. Gemessen bei
  4 200 Zeichen: Index etwa 1,4 × Daten, Lookup etwa 0,1 ms statt 2 µs. Die
  Ergebnisse bleiben korrekt. Für lange Texte sind TOKEN, TRIGRAM und
  FULLTEXT gedacht; VALUE-Indizes auf langen Texten oder Blobs werden nicht
  empfohlen.

### 12.2 Aufbau und Frische

`build_indexes(index_ids=None, commit=False)` liest BASE-Felder über einen
selektiven Decoder und Extensions direkt aus Sparse-Blöcken (neuester Block
gewinnt, Tombstones und Deletes werden beachtet) und schreibt je Index und
Lane einen Schlüsselbaum (Seiten und Wurzel, Abschnitt 7.8). Ein Index ist
**frisch**, wenn sein SECONDARY-Block (die Wurzel) jünger ist als der
jüngste Datenblock der Lane
(`index_is_fresh`). Die direkten Index-APIs (`find_u`/`find`,
`index_candidates`) lösen bei einem Index ohne gebauten Teil (oder über
`overlay_max_bytes`) `StaleIndexError` aus; sonst antworten sie über das
Overlay. Mit `maintenance = "auto"` wird ein Index ohne gebauten Teil – nie
gebaut, oder durch Löschen und Kompaktierung einer Lane ohne Basis – beim
nächsten Commit mit Daten der Lane gebaut, unabhängig von der Rebuild-Schwelle
und nicht durch einen Bulk-Commit verschoben (nur die rote Limiter-Zone
verschiebt ihn). `index_candidates(TRIGRAM, "contains", …)` mit einem
Muster kürzer als `ngram` ergibt `ValueError` (der Index kann es nicht
eingrenzen; SQL scannt dann). Ein einzelner TOKEN-/FULLTEXT-Term beantwortet
`COUNT(*)` nur bei frischem Index direkt aus den Postings
(`index_candidate_estimate(...)["exact"]`); veraltet zählt die Abfrage
über das Overlay.
Der Planer der QueryEngine verwendet nur frische Indizes und weicht sonst auf
einen Scan aus; die BM25-Suche (`search`) verlangt einen frischen
FULLTEXT-Index. `verify` meldet gebaute Indizes mit ausstehenden
Änderungen (ein definierter, nie gebauter Index wird nicht aufgeführt).

**Speicher beim Aufbau (externe Sortierung, ab 0.28.2):** Alle gewählten
Indizes entstehen aus **einem** Scan. Schlüssel und Postinglisten werden im
Arbeitsspeicher gesammelt (je Referenz 4 Byte, ab u ≥ 2^32 8 Byte, ab
u ≥ 2^64 exakte BigInt-Werte); die DATA-Blöcke werden dabei geprüft, aber am
Block-Cache vorbei gelesen. Überschreitet der geschätzte Bedarf aller
Sammlungen `[index] build_memory_bytes` (Standard 512 MiB), werden sie in
Schlüsselordnung sortiert als **Läufe** in eine anonyme temporäre Datei
geschrieben (System-Temp-Verzeichnis, sonst Ordner der Datenbank; CRC je
Lauf, nach Abbruch bleibt nichts zurück) und danach leer weitergeführt
(`db/idxspill.rs`). Beim Schreiben der Schlüsselbäume werden die Läufe und
der Rest im Speicher schlüsselweise gemischt (Listen eines Schlüssels aus
mehreren Läufen nach Dokument-`u`). Spitzenbedarf: das Budget plus je Lauf
ein Lesefenster (zusammen ein Viertel des Budgets), der Schreibpuffer der
Datei (8 MiB) und die Posting-Liste des gerade geschriebenen Schlüssels.
Ein Lauf liest je Schlüssel nur den Kopf; ein Schlüssel mit
mindestens 65 536 Einträgen über mehrere Läufe wird Eintrag für Eintrag aus
den Lesefenstern in den Kodierer gemischt, ohne dass seine Teile dekodiert
gleichzeitig im Speicher liegen. Die geschriebenen Schlüsselbäume sind byte-gleich.
TRIGRAM- und Token-Schlüssel entstehen als Ausschnitte des normalisierten
Texts statt als eigener String je Position.
Das Ergebnis meldet `spill_rounds` und `spill_bytes`; `passes` ist immer 1.
Die in 0.28.0 bei knappem Arbeitsspeicher nötigen mehreren Scans und die
Ablehnung eines Index über dem physischen Arbeitsspeicher mit `MemoryError`
entfallen.

Nicht im Budget enthalten (strukturelle Grenzen, Abschnitt 9.8):
Deduplikationsmengen proportional zur Zahl der Records einer Realm/Lane,
wenn sich Dense-Segmente überlappen oder Deletes existieren; die Abbildung
der Sparse-Overlays indizierter BASE-Felder; die Menge der Sparse-Zellen
indizierter Extension-Felder; die Posting-Liste des gerade geschriebenen
Schlüssels (ein Schlüssel mit sehr vielen Dokumenten, z. B. ein häufiges
Token, ≈ 8 Byte je Dokument; der Dokumentlängen-Schlüssel eines
FULLTEXT-Index hält einen Eintrag je Dokument). Scheitert eine Allokation
dennoch, ergibt das `MemoryError` statt eines Prozessabbruchs, soweit das
Betriebssystem die Anforderung ablehnt (unter Linux kann der OOM-Killer
vorher eingreifen). Die Datenbank bleibt in jedem Fall unverändert im
letzten committeten Zustand.

Der Index eines Realms, der nur Sparse-Daten enthält, wird geschrieben und
gilt als frisch; Sparse-Zustände von BASE-Feldern (inkl. Tombstones und
reiner Sparse-Records) werden beim Indexaufbau berücksichtigt.

**Semantik: Der Index liefert exakt dasselbe wie der Scan.** Der allgemeine
Auswerter (Scan) ist die Referenz; Indexpfade dürfen nur dort verwendet
werden, wo sie identische Ergebnisse liefern:

* **Nur CLASSIC-Records** werden indiziert. Zellen von `u`, die zu einem
  COLUMN-, PACKED- oder CUSTOM-Layout geroutet sind, kommen in keinen Index.
  Der Planer verwendet einen Index nur, wenn alle Layouts des Realms CLASSIC
  sind und das Feld dort unter dem abgefragten Namen erscheint; sonst scannt
  er.
* **Neuester Schreibzustand:** Indexaufbau und Indexpflege übernehmen einen
  Sparse-Zustand eines BASE-Feldes nur, wenn er neuer ist als der Dense-Record
  (Abschnitt 5.2). Ein mit `put_cell` geschriebenes Feld, das der
  gespeicherte Record nicht hat, wird wie im Scan indiziert. Gelöschte Zellen
  (`delete_cell`/`delete_area`/`delete_record`) sind in keinem Index.
* **Gleichheit** folgt Python (`1 == 1.0 == True`); `== NaN` trifft nie
  (auch nicht in Listen oder Dicts).
* **Bereiche** (`between`, `<`, `>` …) schließen `None` und NaN aus;
  Werte, die mit den Grenzen nicht vergleichbar sind (z. B. Text gegen
  Zahl), treffen nicht – ein leeres Ergebnis statt `TypeError`, wie im Scan.
* `u` oberhalb von 2^32 bzw. 2^64 werden in Postings exakt gespeichert.
* Eine Suche liest je Baumebene eine Seite des Schlüsselbaums; jede Seite
  und jeder Posting-Block wird beim ersten Laden per CRC geprüft und an
  ihren Elterneintrag gebunden; eine beschädigte Seite ergibt
  `CorruptionError` (Abschnitt 7.8).

> **Hinweis zur Aktualisierung:** Indizes, die mit NHyDB 0.27.1 oder älter
> gebaut wurden, konnten wegen der behobenen Fehler unvollständige oder zu
> viele Postings enthalten (große `u`, Nicht-CLASSIC-Layouts,
> Extensions nach `delete`). Da Datenbanken von 0.27.1 mit 0.28.x nicht
> öffnen (Abschnitt 7), werden die Indizes nach dem Neuimport mit
> `build_indexes()` neu gebaut.

### 12.3 Kandidaten-API am Kern

| Methode | Zweck |
|---|---|
| `find_u(realm, filters, …)` / `find(…)` | VALUE-Filter (Gleichheit, Bereich) → sortierte `u` bzw. Records (Iterator) |
| `posting_count(index_id, cond)` | Trefferanzahl aus den Blatteinträgen des Schlüsselbaums ohne Dekodierung der Postings |
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
Bitlängen abgeschätzt; Überschreitung ⇒ `MathDomainError`. Die Grenze gilt
nur für Ergebnisse von Rechenoperationen in Formeln; durchgereichte Eingaben
(`u = "x"`), Konstanten, die Realm-Zerlegung und die Realm-Zuordnung sind
nicht begrenzt. Auch die Konstantenfaltung beim Kompilieren hält die Grenze
ein. Es gibt **zwei Einstellungen**, die sich nicht gegenseitig beeinflussen:

| Einstellung | gilt für |
|---|---|
| `[math] math_max_intermediate_bits` in `nhydb_config.toml` | MATH-Layouts einer Datenbank: Der Wert wird beim **Anlegen** eines MATH-Layouts im Layoutzustand eingefroren (`MathLayoutState.max_intermediate_bits`) und gilt danach für Schreiben und Lesen dieses Layouts, unabhängig von späteren Sitzungen und Konfigurationen; `replace_math_spec` (vor dem ersten Schreiben) übernimmt den aktuellen Konfigurationswert |
| `nhydb.math.set_math_max_intermediate_bits(bits)` | prozessweit für **freistehende** Objekte (`MathExpression`, `MathMap`, `dry_run`); wirkt nicht auf Datenbankoperationen; `bits ≥ 1`, sonst `ValueError` |

**Verschachtelungsgrenzen:** Klammern und Rekursionsebenen (Klammer, unäre
Operatoren, `not`, `**`) sind wie in CPython auf 200 begrenzt
(`MathSyntaxError("too many nested parentheses …")` bzw.
„expression too deeply nested …“), der Syntaxbaum auf Tiefe 1000. Lange gleichrangige Ketten
(`x + 1 + 1 + …`) werden n-är und iterativ ausgewertet und sind nicht
begrenzt. Bezeichner und Parameternamen müssen NFC/NFKC-normalisiert sein;
Zahlenliterale folgen den Regeln von CPython (z. B. `0x_ff`, kein Schlüsselwort
direkt nach einer Zahl). Eingabewerte werden wie Python-`int()`
umgewandelt (große Floats exakt, Unterstriche, NaN/inf ⇒ `ValueError`).

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
  `RealmBudgetExceeded`. `realm_auto_limit` begrenzt die **Anzahl** der
  logischen Realms eines MATH-Layouts, nicht deren Größe oder Nummer – eine
  einzelne riesige logische Realmnummer (z. B. ≈ 10^5000) zählt als 1.
* Herkunft von `realm_auto_limit`: `MathMapLayoutSpec(name, mapping,
  realm_auto_limit)` trägt den Wert selbst; `create_math_map_layout(...)`
  nimmt ohne Angabe den Wert aus `[math] realm_auto_limit` der Konfiguration;
  freistehende Objekte (`MathMapLayoutSpec`, `MathRealmMap`, `dry_run`)
  verwenden ohne Angabe die Bibliotheksvorgabe 4096
  (`nhydb.math.DEFAULT_REALM_AUTO_LIMIT`). Der Wert wird mit dem Layout
  gespeichert und beim Öffnen nie durch die Konfiguration überschrieben.
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
`create_realm` auf einer solchen (wie auf jeder belegten) Realm-ID löst
`SchemaError("realm … already exists")` aus.

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
* Universum aus Segmentmetadaten (ohne Marker in der Ausgabe): Realm mit
  genau einem CLASSIC-Layout, `(realm, lane)` ohne Overlays (keine
  Sparse-Zellen, `DELETE`-Blöcke oder Überlappungen), Shapes mit Feldern.
  Dann kommt `COUNT(*)` aus den Segmenten (1 Mio Records 0,76 s → 1,9 ms
  beim ersten Aufruf je Handle mit Prüfsummenprüfung der DATA-Blöcke,
  wiederholt 0,03 ms; Abschnitte 15.1 und 18.5),
  `NOT`/`ALL` brauchen keine Dekodierung, `ORDER BY … LIMIT` baut nur das
  Fenster und `GROUP BY` streamt die Zeilen in 4 096er-Blöcken. Ergebnisse
  und Fehlermeldungen sind identisch zum allgemeinen Pfad (getestet).
* Planer: FIELD-, AND-von-FIELD- und ALL-Ausdrücke werden nur bei Bedarf
  (`explain`) geplant; verschachtelte AND/OR nutzen den Plan von `execute`.
  Vereinigungen mehrerer Postings werden als `u64` sortiert (exakter
  `Nat`-Pfad, sobald ein `u > 2^64` vorkommt).

**Provider:** eingebaut `formula` (Ausdrücke über `k`), `n_sequence`
(arithmetische N-Folge, GZ/N0 übersprungen), `math_layout_inputs`
(Eingabetupel eines MATH-Layouts); zusätzlich Laufzeit-Provider als
Python-Callables, die die Engine erhalten und **lesend** auf `engine.db`
zugreifen dürfen.

**Semantik:** Vergleiche, Gleichheit und Gruppenschlüssel nach Python
(`1 == 1.0 == True`; inkompatible Ordnungsvergleiche ergeben `False` – im
Scan **und** über Indizes, Abschnitt 12.2); `== NaN` trifft nie, Bereiche
schließen `None` und NaN aus; Zeilen mit NaN als Gruppenwert bilden je eine
eigene Gruppe; Gruppen in Einfügereihenfolge; `sum` exakt für Ganzzahlen;
`avg` als korrekt gerundete Division; `ORDER BY` stabil mit fester
`None`-Behandlung, inkompatible Typen ⇒ `QueryPlanError`. Der Fastpath
`CLASSIC_U` liefert dieselben Zeilenformen wie der allgemeine Pfad
(`record`-Hüllen, Strukturspalten); unbekannte Spalten in `select` ergeben
`None`. `IS NULL` trifft nur, wenn das Feld im Record **fehlt** (z. B. eine
nicht gesetzte Extension); ein explizit gespeicherter Nullwert gilt als
vorhanden (`IS NOT NULL`).

**Grenzen:** Ausdrucksbäume (Python-`Q`, JSON-Spezifikation) sind auf
256 Ebenen begrenzt (`QueryPlanError`); gleichartige `&`/`|`-Ketten werden
n-är gesammelt und sind nicht begrenzt. BM25: `k1` muss endlich und > 0
sein (`IndexConfigError`); `@record_n` verlangt ganzzahlige Werte
(`3.0` ja, `3.7` ⇒ `ValueError`); `search(limit=)` nimmt beliebig große
Ganzzahlen.

**Leistungsoptimierungen ohne Semantikänderung:** Zeilen ohne `ORDER BY`
werden erst im LIMIT/OFFSET-Fenster erzeugt; Materialisierungsscans halten
keine Menge aller Records; `STRUCT_RANGE` über Bereiche > 4096 besucht nur
die gespeicherten `u` **des Bereichs** (Kandidaten aus dem Dense-/Sparse-
Intervallindex, Präsenz je `u` wie im Direktpfad; 1 Mio Records,
`u 1..4097`: 284 → 11 ms).

**Thread-Modell:** `query`, `explain_query`, `sql`, `sql_numpy` und die
`QueryEngine`-Methoden laufen ohne GIL; Laufzeit-Provider in Python holen
sich den GIL für ihren Aufruf zurück (Abschnitt 16.7).

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
| Quelle | nur **CLASSIC**-Layouts; Realm als `realm_N`, `realmN`, `rN`, `N` oder eindeutiger Name; Layout als Zahl, `layout_N`, `layoutN` oder eindeutiger Name. `FROM <layout>` umfasst nur die Records, die zu diesem Layout geroutet sind (Routing-Gewinner, Abschnitt 10.2), nicht den ganzen Realm |
| WHERE | `NOT` > `AND` > `OR`, Klammern; `IS [NOT] NULL`, `BETWEEN … AND …`, Vergleiche mit Literal (`= NULL` verboten), Infix `CONTAINS`, `ICONTAINS`, `TOKEN`, `FULLTEXT`, `PHRASE`; Funktionen `TOKEN`, `TOKENS_ALL`, `TOKENS_ANY`, `CONTAINS`, `ICONTAINS`, `FULLTEXT`, `FULLTEXT_ANY`, `PHRASE`, `EXISTS` |
| Strukturreferenzen | `@u`, `STRUCT.u` (`realm`, `u`, `v`, `lane`, `n`, `record_n`, `layout_id`, `layout_kind`, `layout_name`, `object`) |
| Parameter | `?` (positionsnummeriert) und `:name`; werden beim Parsen eingesetzt |
| Lexik | Kommentare `--` und `/* */`, Strings `'…'` mit `''`, Identifier `"…"`/`` `…` ``, Unicode-Ziffern nach CPython-Regeln |
| Nicht implementiert | `DISTINCT`, `HAVING` (Fehler „… is not implemented in NHySQL“), alle schreibenden Anweisungen |

Regeln: Feld- und Aliasauflösung case-insensitiv (casefold); nicht
aggregierte Felder müssen gruppiert sein; `GROUP BY` ohne Aggregat ist ein
Fehler; `relevance` nur ohne Aggregat und nur mit `ORDER BY relevance DESC`
(→ BM25-Suche, Standard-`LIMIT 20`); `COUNT(*)` allein nutzt den Count-Pfad.
Fehlertexte enthalten Zeile, Spalte und `near`. Klammern und `NOT` sind auf
256 Ebenen begrenzt (`SQLSyntaxError`); `AND`/`OR`-Ketten sind n-är und nicht
begrenzt. `IS NULL` hat die Semantik aus Abschnitt 14.3 (Feld fehlt).

Ergebnis: `{query_version, language: "NHySQL-0.24", statement, source,
columns, rows, returned, matched, optimizer, …}`; `EXPLAIN` liefert den
Plan und bei großen Schätzungen dieselben `hints` wie `explain()`.

```python
db.sql("SELECT status, COUNT(*) AS n FROM records GROUP BY status ORDER BY n DESC")["rows"]
# [{'status': 'idle', 'n': 667}, {'status': 'active', 'n': 333}]
db.sql("SELECT id, relevance FROM records WHERE FULLTEXT(body, :q) "
       "ORDER BY relevance DESC LIMIT 5", {"q": "quick fox"})
db.sql_numpy("SELECT id, temp FROM records WHERE id BETWEEN ? AND ?", [3, 7])
# {'id': array([3, 4, 5, 6, 7]), 'temp': array([23., 24., 25., 26., 20.])}
```

### 14.5 Große Ergebnismengen (ab 0.28.2)

`sql()` bleibt unverändert: Es baut das ganze Ergebnis, mit denselben Zeilen,
Typen, Reihenfolgen und Fehlern wie zuvor. Für sehr große Ergebnisse gibt es
weitere Wege. Die Planerabkürzungen von 0.28.2 liefern dieselben Ergebnisse
wie der allgemeine Weg:
- Top-k für `ORDER BY … LIMIT`;
- Aggregate und Ordnung aus einem abdeckenden Wertindex. Aggregate nehmen
  diese Abkürzung nur, wenn alle Shapes das Feld einheitlich als Ganzzahl
  bzw. utf8 führen und keine Sparse-Zellen bestehen;
- Feldprädikate im u-Raum;
- selektives Dekodieren.

* **`db.sql_iter(text, parameters=None, *, lane=0)`**: Zeilen wie
  `sql()["rows"]`, abschnittsweise (je 4096) gebaut. Der Speicher der
  Feldabfragen hängt nicht von der Ergebnisgröße ab. Es gilt die
  Iterator-Semantik von `select`/`find`:
  - Fehler kommen erst beim ersten `next()`.
  - Schreiben zwischen zwei Schritten ist erlaubt; ein Abschnitt zeigt den
    Stand beim Bau seiner Zeilen.
  - Ein Datensatz, der vor dem Bau seines Abschnitts gelöscht wurde, wird
    übersprungen.
  - Nach `rollback()` desselben Objekts folgt `NHyDBError`.
  - `db.sql_numpy` nutzt denselben Strom.
* **Ergebnisdateien**: `db.sql_to_file(…, path=None, compress=None)`
  schreibt die Zeilen in einen Ergebniscontainer `.nhyc` und liefert ein
  `nhydb.ResultFile`. Aufbau, alle Zahlen little endian, CRC = CRC-32C:
  - **Header:** `"NHYC"`, Version 1, Flags 0, Art, Metadaten, CRC.
  - **Chunks:** `"CK"`, Codec (roh/zlib), Einträge, Längen, Kopf-CRC,
    Nutzlast, Nutzlast-CRC.
  - **Footer:** `"FT"`, 0 0, Anzahlen, Metadaten, CRC.
  - **Trailer:** Footer-Offset und `"NHYC"`.
  - **Einträge:** je Zeile eine Map im kanonischen Wertecodec. Jede
    Abweichung ist `CorruptionError`.

  Das `ResultFile` bietet:
  - Iteration, `fetchall()`, `to_numpy()`, `pin()`/`unpin()`, `delete()`,
    `close()`.
  - **Ohne `path`** liegt die Datei im Ergebnisverzeichnis `[results]
    temp_dir` (Standard `<System-Temp>/nhydb-results`, unabhängig vom Ort
    der Datenbank) und wird beim Schließen gelöscht, außer sie ist gepinnt.
  - **Mit `path`** gehört sie dem Aufrufer und wird nie überschrieben,
    auch nicht bei gleichzeitigem Anlegen.
* **Hintergrund**: `db.submit_sql(…)` führt die Abfrage in einem eigenen
  Thread auf einem Lese-Schnappschuss aus (Stand des letzten Commits) und
  liefert ein `nhydb.ResultJob`: `done()`, `rows_written`, `cancel()`,
  `wait(timeout)`. Für `timeout` gilt: `inf` = ohne Grenze, NaN =
  `ValueError`. Ein interner Fehler im Auftrag endet als Fehler von
  `wait()`.
* **Besitz und Aufräumen**:
  - Verwaltete Dateien heißen genau `nhydb-<pid>-<hex>-<nr>.nhyc`; dieses
    Muster ist im Ergebnisverzeichnis reserviert.
  - Ihr Besitzer hält eine geteilte Dateisperre ohne Lücke: vom Anlegen
    (verdeckt angelegt, gesperrt, dann benannt) über Abschließen und
    Umbenennen bis zum Schließen, auch über `pin()`/`unpin()`.
  - `nhydb.cleanup_results(directory=None)` löscht nur Dateien mit diesem
    Muster und nur unter seiner eigenen exklusiven Sperre. Das ist sicher
    auch gegenüber anderen Prozessen und gegenüber Dateien des Aufrufers
    mit ähnlichem Namen.
  - Kein Ablauf nach Zeit.
* **Externe Sortierung**: `ORDER BY`, mit und ohne LIMIT, sortiert bei `sql_iter`,
  `sql_numpy`, `sql_to_file` und `submit_sql` über sortierte Läufe, sobald
  die Schlüssel `[results] sort_memory_bytes` übersteigen (Standard 256 MiB,
  mindestens 64 KiB).
  - **Läufe:** anonyme temporäre Dateien im Ergebnisverzeichnis, ohne Namen
    und nach einem Absturz ohne Rest. Sie zählen bis zum Ende des Mischens
    zu `[results] budget_bytes`.
  - **Geltungsbereich:** jedes `ORDER BY` dieser Wege, mit und ohne
    LIMIT, in jeder Realm und jedem Layout. Die Ausnahme ist NaN über dem
    Budget, siehe unten. Im schnellen CLASSIC-Weg enthalten die Läufe
    Schlüssel und u. In allen anderen Wegen, etwa mit Löschungen,
    Einzelzellen, mehreren Shapes oder anderen Layouts, enthalten sie die
    fertigen Zeilen, und jeder Datensatz wird nur einmal gelesen.
  - **Reihenfolge:** immer die von `sql()`. Unvergleichbare Werte einer
    Spalte geben denselben Fehler wie `sql()`.
  - **NaN:** Innerhalb von `sort_memory_bytes` wird mit dem Verfahren von
    `sql()` sortiert, mit identischer Reihenfolge. Über dem Budget folgt
    `QueryLimitError`, weil NaN keine Ordnung hat, die sich über Läufe
    mischen ließe.
    - Begründung: Eine feste NaN-Ordnung wie bei PostgreSQL („größer als jede
      Zahl“) hätte die bisherige Sortierung von `sql()` geändert oder
      `sql()` und Streaming auseinanderlaufen lassen. Ein Fehler statt einer
      stillen Abweichung ist die einzige konsistente Lösung ohne Rückfall in
      den Speicher.
  - **Mischen:** Die Endmischung nimmt höchstens 128 Läufe zu je etwa
    128 KiB. Mehr Läufe werden vorher stufenweise zusammengeführt, Stufe 0
    je 32 und höhere Stufen je 128. Eine Stufe braucht zeitweise den Platz
    der gerade zusammengeführten Läufe zusätzlich im Budget.
  - **Absturzreste:** Ein Prozess, der beim Anlegen eines Laufs stirbt, kann
    eine leere verdeckte Datei hinterlassen. `cleanup_results()` entfernt
    sie (Unix).
  - **Fehlerfälle:** siehe die Tabelle unten. Es gibt keinen
    Rückfall in den Speicher.

  | Meldung beginnt mit / Klasse | Ursache | Abhilfe |
  |---|---|---|
  | „ORDER BY without enough memory needs sort runs in the result directory …“ – `PermissionError`/`OSError` (errno, Pfad) | Ergebnisverzeichnis nicht beschreibbar, Platte voll | Schreibrecht bzw. Platz schaffen, `[results] temp_dir` auf ein anderes Verzeichnis setzen, `nhydb.cleanup_results()` |
  | dieselbe Einleitung – `DatabaseSizeLimitError` | `[results] budget_bytes` erschöpft | Budget erhöhen, aufräumen, Abfrage mit LIMIT/WHERE eingrenzen |
  | „ORDER BY …: the sort keys exceed [results] sort_memory_bytes … NaN …“ – `QueryLimitError` | NaN in einer Sortierspalte über dem Budget | NaN herausfiltern, eingrenzen, `sort_memory_bytes` erhöhen |
  | „query result too large … max_result_bytes …“ – `QueryLimitError` | `sql()`/`execute()`/`search()`/`sql_numpy()` würde mehr als `[query] max_result_bytes` halten | `sql_iter()`/`sql_to_file()`/`submit_sql()` nutzen, LIMIT/WHERE/Aggregat, Grenze erhöhen (0 = aus) |
* **Speichergrenze der sammelnden Aufrufe:** `[query]
  max_result_bytes`, Standard 4 GiB, 0 = keine Grenze.
  - **Wen sie betrifft:** `sql()`, `QueryEngine.execute()`/`search()` und
    `sql_numpy()`. Sie geben das ganze Ergebnis auf einmal zurück, es liegt
    also zwangsläufig im Speicher.
  - **Was gemessen wird:** die geschätzten Bytes der gesammelten Zeilen,
    Sortierschlüssel und Aggregatgruppen, laufend während des Sammelns. Bei
    Überschreitung folgt sofort `QueryLimitError`. Bei `sql_numpy()` zählen
    die Spaltenwerte (etwa 8 Byte je Wert plus Zeichenketten). Die
    Python-Objekte von `sql()` brauchen zusätzlich etwa das 2- bis 3-Fache.
    Gruppen eines GROUP BY, auch in Streaming-Aufrufen, werden ganz
    gesammelt und unterliegen dieser Grenze.
  - **Hinweis:** Das ist eine bewusste Verhaltensänderung von `sql()`, das
    vorher unbegrenzt war. Für große Ergebnisse sind die Streaming-Wege da.
  - **Außerdem:** `find_u()` und der `find()`-Iterator (16 Byte
    je gefundenem u) sowie `ResultFile.fetchall()` und
    `ResultFile.to_numpy()`, die die ganze Ergebnisdatei laden. Das Iterieren
    über eine `ResultFile` bleibt unbegrenzt. `ORDER BY … LIMIT` in `sql()`
    hält auch außerhalb des schnellen CLASSIC-Wegs nur das Top-k-Fenster,
    solange die Sortierschlüssel total geordnet sind (Zahlen ohne NaN oder
    Zeichenketten); ein kleines LIMIT scheitert dann nicht an der Grenze.
* **Grenze je Abfrage** (optional): `[query] max_result_rows` (Standard 0 =
  keine Grenze).
  - Abfragen, deren geschätzte Ergebniszeilen (durch LIMIT begrenzt) darüber
    liegen, lehnt `QueryLimitError` vor dem Lauf ab.
  - Die Schätzung ist grob: Ein Filter ohne Index schätzt die ganze Realm.
  - COUNT, Aggregate und `sql_iter` sind ausgenommen.
  - `explain()` und `sql("EXPLAIN …")` zeigen ab 1 Mio. geschätzten Zeilen
    bzw. über der Grenze `hints`.

---

## 15. Integrität, Recovery und Sicherheitsmechanismen

### 15.1 Prüfsummen und Strukturprüfungen

| Prüfung | Zeitpunkt |
|---|---|
| Superblock-CRC32, Magic, Version | jedes Öffnen |
| Header-CRC der Blöcke im gescannten Bereich (ohne Verzeichnis: alle; mit Verzeichnis: die Blöcke hinter der Wurzel) | jedes Öffnen (ein Scan) |
| Block-IDs steigen mit der Position in der Datei | Öffnen (gescannter Bereich), `verify`, `inspect_tail` |
| Payload-CRC von SYSTEM, DB_CONFIG, DELETE, CHECKPOINT der gescannten Blöcke | beim Öffnen |
| Präfixprüfsumme (CRC-32C, Abschnitt 7.7) der gescannten DATA_DENSE- und SPARSE_CELLS-Blöcke (`D28D`/`SP28`) | beim Öffnen (fail closed, `CorruptionError` „… prefix checksum mismatch“); ein nur über das Präfix geprüfter Block gilt **nicht** als verifiziert |
| Payload-CRC von Datenblöcken (DATA, SPARSE) | vor der ersten Nutzung je Handle (ganzer Block, danach Teil-Lesen), in `verify`, in der Kompaktierung (bei Fehler wird das Staging verworfen) und beim Indexaufbau |
| Bindung eines über einen Verzeichniseintrag geladenen Blocks: Kopf (Typ, Block-ID, Lane, Länge) und Präfix (Tag, Realm, Shape, `u`-Bereich, Zähler, Präfixlänge) | beim ersten Laden des Blocks (sonst `CorruptionError`) |
| Verzeichnisblöcke: CRC; Seite gegen ihre Zusammenfassung im Lauf; Wurzel nennt jeden Lauf und jede Seite genau einmal | beim Lesen der Wurzel/Läufe (Öffnen) bzw. jeder Seite (auch aus dem Cache) |
| Verzeichnis gegen Datenblöcke (Einträge, Kennzahlen, Überlagerungsflags, DELETE-Einträge) | `verify(deep=True)` |
| Payload-CRC der Schlüsselbaum-Seiten und Posting-Blöcke (SECONDARY, SECONDARY_PAGE) und Bindung an den Elterneintrag (Offset, Block-ID, Typ, Tag, Index, Stufe, Lane) | beim ersten Laden der Seite bzw. des Posting-Blocks |
| Schlüsselbäume: Bindung, Schlüsselordnung, Schlüsselzahl, jede Posting dekodierbar und mit passender Anzahl | `verify(deep=True)` (ohne den Seiten-Cache zu füllen) |
| DATA-Präfix: Ende der Offsettabelle = Payloadende, Shape vorhanden und zum Realm gehörig | beim Öffnen (aus dem Lesefenster des Open-Scans; bei großen Blöcken ein zusätzlicher kleiner Lesezugriff) |
| Präfixe, Offsettabellen, Sparse-Streams, Gruppenzahl | `verify`, mit `deep=True` vollständige Dekodierung aller Records |
| Plan: HMAC, AES-GCM-Tag, Schema-Digest (HMAC) des Klartexts, Contracts, Generation | jedes Öffnen |
| R0-Kette: unveränderliche Zellen, Generation +1, v9 | jedes Öffnen |
| MATH-Zustände (keine Fremddefinitionen in MATH-Realms, keine Doppelbelegung) | Öffnen und `health` |

Alle Längen- und Offsetberechnungen aus Dateiinhalten sind überlaufgeprüft
(`checked_add`, `try_from`); Zähler aus der Datei (Gruppen, Postings) werden
gegen die Payloadlänge plausibilisiert, Allokationen mit `try_reserve`
angefordert, zlib-Ströme mit einer Größengrenze entpackt, JSON-Metadaten bis
Tiefe 256 gelesen. Beschädigte oder manipulierte Dateien ergeben
`CorruptionError` (bzw. `MemoryError` bei echtem Speichermangel), nie
`IndexError`, `PanicException` oder einen Prozessabbruch.

**Routing-Präfixe:** Für Blöcke, die das Verzeichnis abdeckt, stammt die
Routing-Information (Realm, Shape, `u`-Bereich, Zähler) aus den
CRC-geprüften Verzeichnisseiten; beim ersten Laden muss der Block selbst
(Payload-CRC, Kopf und Präfix) seinem Eintrag entsprechen, sonst
`CorruptionError` – ein CRC-gültiger, mit einem anderen vertauschter Block
liefert daher nicht still die ältere Version. Für Datenblöcke im gescannten
Bereich (hinter der Verzeichniswurzel bzw. alle ohne Verzeichnis) schützt
seit 0.28.2 die **Präfixprüfsumme** (Abschnitt 7.7) Realm, Shape,
`u`-Bereich und Zähler: Ein gekipptes Präfixbyte ergibt beim Öffnen
`CorruptionError`, ohne dass die ganze Payload gelesen wird. Weil die Präfixprüfsumme eine
andere CRC als die Payload-CRC ist, scheitert ein Präfix mit neu
berechneter Präfixprüfsumme weiter an der Payload-CRC beim ersten Laden.
Ein verschobenes Overlay kann damit keinen älteren BASE-Wert still
zurückgeben. Bei DATA-Präfixen muss außerdem die Shape existieren und zum
Realm des Segments gehören.

**Blöcke im Format 0.16** (`D10D`/`SP10`, ohne Präfixprüfsumme) werden
nicht gelesen: Ihr Tag ergibt beim Öffnen `CorruptionError` (falsches Tag).

**Verbleibende Grenze:** Für Blöcke im Format 0.28.2 sind die Routing-Präfixe
durch die Präfixprüfsumme geschützt. Nicht erkannt wird ein
absichtlich gefälschter Block, dessen Prüfsummen (Präfix und Payload) neu
berechnet wurden – das liegt außerhalb dessen, wogegen Prüfsummen schützen
(Abschnitt 19.1, „Warnung: gefälschte Verzeichnisse“).

### 15.2 Tail-Recovery

Bytes hinter der Commit-Grenze werden beim Öffnen – erst nach erfolgreicher
Prüfung von R0, Schlüssel, Plan und Schema – abgeschnitten; ihre Anzahl steht
in `db.recovered_tail_bytes`. Abgeschnitten wird nur, wenn die Commit-Grenze
eindeutig ist (beide Superblöcke gültig und konsistent) und die Sperre
gehalten wird; sonst schlägt das Öffnen fehl, ohne die Datei zu ändern
(Abschnitt 8.4). Committete, CRC-gültige Daten werden nie stillschweigend
abgeschnitten.

### 15.3 Wartungsfunktionen

| Funktion | Beschreibung |
|---|---|
| `verify(deep=False)` | Blockstruktur bis Dateiende; prüft nach einem Schaden weiter und meldet **jeden** beschädigten Block; Ergebnis `{ok, scanned_blocks, physical_records, error_count, errors, warnings, …}`; gebaute Indizes mit ausstehenden Änderungen stehen in `warnings` (kein Fehler; ein nie gebauter Index wird nicht aufgeführt); ein „degraded“ Blockverzeichnis und ein wegen eines beschädigten Blocks abgebrochener Verzeichnis-Flush (`last_flush_error`) ergeben eine Warnung; `deep=True` vergleicht zusätzlich das Blockverzeichnis mit den Datenblöcken (Abschnitt 7.10) und durchläuft alle Schlüsselbäume der Indizes (Abschnitt 7.8) |
| `health(deep=False)` | Checks `container`, `layouts`, `spacemap`, `layout_routing`, `math_layouts`, `definitions`, `indexes`, `checkpoint`; Gesamtstatus `healthy`/`warning`/`unhealthy`; ein veralteter Index ergibt `warning` |
| `stats()` | Datei, Format, Realms, Shapes, Layouts, Extensions, Indizes (inkl. `kind`, `options`, `distinct_values`), `max_db_size_bytes`, MATH-Layouts, `block_cache` und `directory` (Abschnitt 9.8) |
| `checkpoint()` | CHECKPOINT-Block + Commit |
| `NHyDB.inspect_tail(path)` | prüft die Datei ohne Öffnen (Header-, Payload-CRC); meldet zusätzlich beide Superblöcke (`superblocks`), die Commit-Grenze (`committed_end`), `superblock_problem`, `uncommitted_bytes`, `damaged_blocks` und intakte Blöcke hinter einer Schadstelle (`later_valid_blocks`), seit 0.28.2 auch `damaged_directory_blocks` und `directory_damaged_in_use` |
| `NHyDB.repair_file(path, *, apply=False, make_backup=True, rebuild_indexes=True, allow_data_loss=False, keep_uncommitted=False, …)` | stellt einen eindeutigen, konsistenten Zustand her (Abschnitt 15.4) |
| `NHyDB.restore_backup(backup, path, …)` | spielt eine Sicherung von `repair_file`/`compact_file` (DB + Plan) geprüft wieder ein (Abschnitt 15.5) |
| `NHyDB.compact_file(path, *, output=None, make_backup=True, rebuild_indexes=True, batch_records=10000, …)` | schreibt den logischen Zustand neu |

### 15.4 Repair

`repair_file` nimmt (bei `apply=True`) zuerst die Schreibsperre, untersucht die Datei
(`inspect_tail`, beide Superblöcke) und bestimmt ein **sicheres** Ende:
übernommen werden nur CRC-gültige Blöcke, deren R0-Generation im Layoutplan
liegt und deren Datenblöcke auf veröffentlichte Realms, Shapes und Indizes
verweisen. Grundsätze:

* **Committete Daten werden nie stillschweigend geopfert.** Lägen intakte,
  committete Blöcke hinter einer Schadstelle und würden abgeschnitten,
  bricht `repair_file` mit `CorruptionError` ab und ändert nichts. Nur mit
  `allow_data_loss=True` werden sie verworfen; sie stehen dann in
  `lost_blocks`.
* **Unkommittierte Blöcke** hinter der Commit-Grenze werden standardmäßig
  verworfen (`discarded_uncommitted_blocks`); mit `keep_uncommitted=True`
  werden intakte unkommittierte Blöcke übernommen
  (`kept_uncommitted_blocks`).
* Ist die Commit-Grenze mehrdeutig (ein Superblock beschädigt oder
  inkonsistent) und folgen vollständige Blöcke, verlangt `repair_file` eine
  ausdrückliche Entscheidung: `keep_uncommitted=True` (behalten) oder
  `allow_data_loss=True` (verwerfen); sonst `CorruptionError`.
* Ist die Datei kürzer als die Commit-Grenze, meldet das Ergebnis
  `missing_committed_bytes`; die neue Grenze ist das letzte gültige
  Blockende.
* **Beschädigtes Blockverzeichnis:** Wegen beschädigter Verzeichnisblöcke
  schneidet `repair_file` nichts ab; es verwirft das Verzeichnis
  (`dir_root = 0`, verlustfrei, weil alle Angaben auch in den Datenblöcken
  stehen); die Datenbank öffnet danach mit vollständigem Scan.
* Beide Superblöcke werden mit neuen Generationen geschrieben (erst der
  inaktive Slot, je mit Sync), danach wird gekürzt.
* `apply=False` (Standard) ändert nichts und berichtet die geplante Aktion
  (`action`, `cut_reason`, `plan_error`, `damaged_blocks`, `lost_blocks` …).
* Mit `make_backup=True` werden vorher (unter eindeutigen Namen, auch bei
  mehreren Sicherungen je Sekunde) die DB-Datei als
  `<db>.bak_<zeitstempel>` und der Layoutplan als
  `<db>.bak_<zeitstempel>.layout.enc` dauerhaft gesichert
  (`backup`, `backup_layout_plan`).

Sidecar-Pfade und Konfiguration werden durchgereicht.

### 15.5 Kompaktierung

* **In-place** (`output=None`): neue Datei mit identischer R0-Identität (ID,
  Generation) und denselben Sidecars; nur die `.nhydb` wird atomar ersetzt;
  bei `make_backup=True` dauerhafte Sicherung der DB-Datei als
  `<db>.before_compact_<zeitstempel>` und des Layoutplans als
  `<db>.before_compact_<zeitstempel>.layout.enc`.
* **Export** (`output=…`): neue UUID, eigene Sidecars neben der Zieldatei.
* Ablauf: Dense-Records mit gespeicherten BASE-Rohwerten (durch einen
  `DELETE`-Block verdeckte Records werden nicht übernommen), neueste
  Sparse-Zustände ohne die vom Dense-Record verdeckten (Tombstones nur
  dort, wo sie Dense-Werte verdecken), jeweils nach dem Dense-Record ihres u
  geschrieben, MATH-Zustände 1:1,
  Indexneuaufbau, Checkpoint, Deep-Verify der Zieldatei, erneutes Öffnen mit
  Deep-Verify, atomarer Austausch.
* **Speicherbegrenzt:** BASE-Records werden als Rohbytes übernommen (gleiche
  Shape; die Tiefenprüfung des Ziels dekodiert jeden Record erneut),
  Tombstones nur bei Bedarf gegen Shape/Extensions des Records geprüft,
  Extension- und Sparse-Zellen in Blöcken zu 50 000 geschrieben. Der
  Arbeitsspeicher hängt von `batch_records` ab, nicht von der DB-Größe
  (1 Mio KQ-Records mit Overlays: Spitzen-RSS 1 880 → 67 MiB, 3,5 → 1,2 s;
  2 Mio: 91 MiB).
* **Sparse-Zellen (seit 0.28.2):** Die Sparse-Zellen werden nicht mehr in
  einer Abbildung aller lebenden Zellen gesammelt (≈ 300 Byte je Zelle;
  große Sparse-Datenbanken ließen sich nicht kompaktieren). Eine
  speicherbegrenzte Zusammenführung (`db/merge.rs`) liest jeden SPARSE-Block
  als Strom durch ein begrenztes Lesefenster (alle Fenster zusammen 32 MiB)
  und führt die Blöcke nach `(u, v, Block-ID)` neben den Dense-Records
  derselben Realm/Lane zusammen (bei gleicher Zelle gewinnt der zuletzt
  geschriebene Block, wie im Lesepfad). Werte über 256 Byte werden erst
  gelesen, wenn ihre Zelle gewinnt; eine überholte große Version wird nie
  gelesen. Überlappen an einem `u` mehr als 4 096 Blöcke, werden Gruppen
  von Blöcken zu sortierten Läufen zusammengeführt (externe
  Zusammenführung); alle Läufe liegen in **einer** anonymen temporären
  Datei neben dem Ziel (unter Unix ohne Namen, unter Windows
  delete-on-close; ein Dateideskriptor; nach Absturz oder `kill -9` bleibt
  nichts zurück). Laut `CHANGELOG.md` gemessener Spitzenspeicher
  ≈ 55–60 MB unabhängig von den Daten (40 Mio Zellen, 3,6-GB-Datei: 57 MB;
  zuvor ≈ 1,4 GB für 5 Mio Zellen); 300 Versionen einer 1-MiB-Zelle:
  630 → 81 MB. Die Quelle wird mit höchstens 16 MiB Block-Cache gelesen;
  das Ziel wird einmal vollständig zu einem Verzeichnislauf zusammengeführt.
  Ist das Verzeichnis der Quelle beschädigt, kompaktiert NHyDB aus einem
  vollständigen Scan.
* **Plattenplatz:** Mit `make_backup=True` wird vorher geprüft, ob der freie
  Platz für die Sicherungskopie reicht. Spitzenbedarf: Original + Ziel
  (+ Sicherung), bei externer Zusammenführung zusätzlich bis zur Menge der
  Sparse-Daten für die temporären Läufe.
* In-place-Kompaktierung funktioniert auch mit MATH-Daten; Realm-Defaults
  (`default_shape_id`/`base_reserve_to`) und Tombstones auf BASE-Feldern
  bleiben erhalten.
* Die Sperre wird über Quelle, Staging-Datei und Austausch gehalten.
* Wird eine Kompaktierung hart beendet, bleibt die Staging-Datei
  `<db>.compacting` liegen. `health()` meldet sie dann als Prüfung
  `compaction_leftover` (Warnung, mit Pfad, Größe und `removable`). Ein
  Schreib-Handle mit eigener Sperre entfernt sie beim Öffnen (eine laufende
  Kompaktierung hielte diese Sperre, es kann also keine laufen);
  `cleanup_compaction()` entfernt sie ausdrücklich, nur mit eigener Sperre,
  und nur eine reguläre Datei genau dieses Namens.
* Schlägt die Kompaktierung fehl, werden nur die von ihr selbst angelegten
  Dateien entfernt; die Quelle bleibt unverändert.

**Sicherungen wieder einspielen.** Sicherungen von `repair_file` und
`compact_file` bestehen aus `<backup>` und `<backup>.layout.enc`; der
Schlüssel bleibt unverändert. `NHyDB.restore_backup(backup, path)` (CLI
`nhydb restore`) nimmt die Sperre von `path`, prüft, dass die Sicherung mit
dem Plan der Sicherung und dem Schlüssel von `path` öffnet und zu dieser
Datenbank gehört, kopiert beide Dateien dauerhaft neben die Ziele und
tauscht sie aus (erst den Plan, dann die DB). Von Hand: `<backup>` auf den
DB-Pfad und `<backup>.layout.enc` auf den Planpfad kopieren, während die
Datenbank geschlossen ist.

### 15.6 Sicherheitsmechanismen und ihre Grenzen

| Mechanismus | Schützt gegen | Schützt **nicht** gegen |
|---|---|---|
| AES-256-GCM + HMAC des Layoutplans | unbemerkte Manipulation oder Vertauschung von Schema/Plan; Lesen des Schemas ohne Schlüssel | Lesen der **Nutzdaten** in `X.nhydb` – diese sind **nicht verschlüsselt** |
| Bindung R0 ↔ Plan (Generation, Schema-Digest mit Schlüssel) | Kombination nicht zusammengehöriger Dateien; Bestätigen eines geratenen Schemas ohne Schlüssel | Verlust der Schlüsseldatei (DB dann nicht mehr zu öffnen) |
| Dateinamensbindung (R0,v2) | versehentliches Öffnen umbenannter Dateien | – |
| Schreibsperre `<db>.lock` | gleichzeitige Schreibprozesse derselben Engine; die Sperrdatei folgt keinem symbolischen Link | Prozesse, die die Sperre umgehen (`lock=False`) oder nicht beachten |
| Größengrenze `DB_CONFIG` | unkontrolliertes Dateiwachstum | – |
| MATH-Bitgrenze | Ressourcenerschöpfung durch Ausdrücke | – |
| CRC32 (Payload, Header), CRC-32C (Präfix der Datenblöcke) | zufällige Bitfehler, Teil-Schreibvorgänge | gezielte Manipulation der Datenblöcke (CRC ist kein kryptografischer Schutz) |

Schlüsseldatei: Unix-Rechte `0600` (Warnung beim Öffnen, wenn Gruppe oder
andere lesen dürfen); unter Windows erbt sie die ACL ihres Verzeichnisses
(Abschnitt 8.3). Geheimschlüssel gehören nie in die Konfigurationsdatei.

**Schlüsselmaterial im Arbeitsspeicher:** Das Schlüsselobjekt wird beim
Freigeben mit Nullen überschrieben, die daraus abgeleiteten Schlüssel für
Plan-MAC und Schema-Digest unmittelbar nach Gebrauch. Nicht garantiert
gelöscht werden die internen Zustände der verwendeten HMAC- und
AES-GCM-Implementierungen sowie temporäre Puffer beim Laden und Anlegen der
Schlüsseldatei (Dateiinhalt, JSON-Objekt, Base64-Text). NHyDB ist kein
Sicherheitsprodukt: Verschlüsselt ist nur der Layoutplan, die Datendatei
`.nhydb` nicht.

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
(`"0.28.2"`, die Produktversion), `FILE_FORMAT_VERSION` (`"0.28.2"`, die
Version des Dateiformats = Engine-Release, das es festgelegt hat), `TYPE_CODES`. `nhydb.query` stellt zusätzlich
`QUERY_VERSION` (`"0.27"`) und `SQL_LANGUAGE_VERSION` (`"NHySQL-0.24"`)
bereit (Abschnitt 18.3).

### 16.2 Öffnen und Anlegen

```python
db = nhydb.NHyDB.create("x.nhydb", *, overwrite=False, layout_path=None, key_path=None,
                        config=None, durability=None, lock=None, lock_timeout_ms=None)
db = nhydb.NHyDB("x.nhydb", *, readonly=False, layout_path=None, key_path=None,
                 config=None, durability=None, lock=None, lock_timeout_ms=None)
```

Alle Optionen sind Schlüsselwortargumente. `nhydb.create(...)` und
`nhydb.open(...)` sind Kurzformen für `NHyDB.create(...)` bzw. `NHyDB(...)`.
Der frühere Parameter `strict` ist entfallen (er hatte keine Wirkung); ein
Aufruf mit `strict=` löst `TypeError` aus. `readonly=True` öffnet ohne Sperre
und ohne die Datei zu ändern (Abschnitt 8.4); `create` lehnt `readonly=True`
ab.

* Alle Pfade (DB, Plan, Schlüssel, Sperre, Konfiguration) werden beim Öffnen
  bzw. Anlegen absolut verankert; ein späteres `os.chdir` wirkt sich nicht
  aus. Leere, gleiche oder auf Verzeichnisse zeigende Pfade für DB, Plan,
  Schlüssel und Sperre werden abgelehnt (`ValueError` bzw.
  `IsADirectoryError`).
* `create(overwrite=True)` legt die neue Datenbank zuerst unter temporären
  Namen an und ersetzt die alte erst danach; Plan- oder Schlüsseldateien
  einer **anderen** Datenbank (andere `db_id`) werden nie gelöscht, sondern
  führen zu `FileExistsError`. Für mehrere gleichnamige Datenbanken mit
  gemeinsamem Schlüsselverzeichnis gibt es den Platzhalter `{db_path_hash}`
  (Abschnitt 17.2). `db_max_size_bytes` muss mindestens 64 KiB
  betragen.
* `lock=` und `lock_timeout_ms=` überschreiben die Einträge aus `[lock]`
  einzeln (ein nicht angegebener Wert kommt aus der Konfiguration).

Attribute: `path`, `closed`, `durability` (les- und schreibbar),
`layout_path`, `key_path`, `max_db_size_bytes`, `next_block_id`,
`last_valid_end`, `committed_end`, `checkpoint_offset`,
`recovered_tail_bytes`, `config_source`, `realms` (`{id: dict}`),
`index_defs` (`{id: IndexDef}`), `math_layouts`, `f` (lesende Dateisicht mit
`seek`/`tell`), `readonly`, `open_warnings` (Liste der Warnungen beim
Öffnen, z. B. Superblock-Rückfall oder zu offene Schlüsseldatei; sie werden
zusätzlich als `RuntimeWarning` ausgegeben). `path` ist absolut.

### 16.3 Python-API (Auswahl nach Bereichen)

| Bereich | Methoden |
|---|---|
| Schema | `create_realm`, `create_shape`, `set_realm_default_shape`, `define_extension`, `define_extensions`, `resolve_v`, `extension_catalog` |
| Layouts | `create_layout`, `create_classic_layout`, `create_column_layout`, `create_packed_layout`, `create_custom_layout`, `register_layout_plugin`, `layout_at`, `layout_candidates`, `route_address`, `layout_diagnostics`, `get_layout`, `plan_layout_evolution`, `evolve_layout`, `logical_address`, `record_anchor` |
| Schreiben | `put_dense_batch`, `put`, `put_extensions`, `put_extension_list`, `put_extension_range`, `put_points`, `delete_cell`, `delete_area`, `delete_record`, `put_object`, `put_objects_batch`, `batch` (`WriteBatch.put_cell`/`delete_cell`/`delete_record`/`delete_area`/`flush`/`discard`) |
| Lesen | `get`, `get_internal`, `get_cell`, `get_cell_n`, `get_object`, `get_record_view`, `get_n`, `get_n_cell`, `get_n_record`, `get_address`, `describe_n`, `occupied_n`, `occupied_from_n`, `range`, `select`, `select_us`, `iter_materialized_sparse_points`, `iter_materialized_cells`, `iter_materialized_record_ns`, `record_n_for_address`, `record_n_from_n` |
| Adressen | `realm_bounds`, `dyn_address`, `cell_address`, `resolve_n`, `set_dyn_address_output` |
| Indizes | `create_index`, `create_token_index`, `create_trigram_index`, `create_fulltext_index`, `build_indexes`, `find_u`, `find`, `posting_count`, `index_candidate_estimate`, `index_candidates`, `fulltext_term_posting`, `fulltext_corpus`, `fulltext_payloads` |
| MATH | `create_math_layout`, `create_math_map_layout`, `replace_math_spec`, `put_math_batch`, `put_math_numpy`, `put_math`, `get_math_cell`, `get_math`, `describe_math` |
| Abfragen | `query`, `explain_query`, `sql`, `sql_numpy`, `compile_sql` |
| System | `get_system_metadata`, `get_system_cell`, `set_max_db_size` |
| Wartung | `commit`, `rollback`, `sync`, `close`, `checkpoint`, `verify`, `health`, `stats`, `compact_file`, `inspect_tail`, `repair_file`, `restore_backup` (statisch) |

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
| `get_value(realm_id, u, v, *, lane=None)` | Wert oder `None`; `v` auch als Spaltenname (Feld-, Extension-, COLUMN-Feldname, `"v<n>"`) wie bei `get_cell` |
| `delete_cell(realm_id, u, v, *, lane=None, commit=False)` | Tombstone; `True`, wenn die Zelle lebte (auch mit gespeichertem `None`), sonst `False` (dann wird nichts geschrieben) |
| `delete_area(realm_id, u_from, u_to, v_from, v_to, *, lane=None, commit=False)` | alle lebenden Zellen des Rechtecks (Grenzen inklusiv) löschen, eine Einheit; liefert die Zahl der gelöschten lebenden Zellen |
| `delete_record(layout_id, object_id, *, lane=None, commit=False)` | Record vollständig löschen (alle lebenden Zellen des Objekts, BASE und Extensions, Dense und Sparse); liefert die Zahl der gelöschten lebenden Zellen (0 = nichts) |

`commit=True` führt dieselbe Commit-Sequenz wie `db.commit()` aus (auch wenn
nichts zu löschen war). `put` schreibt den BASE-Record seines Shapes als
Ganzes, `put_cell` eine Zelle; es gilt der neueste Schreibzustand je Zelle
(Abschnitt 5.2).

**Schreiben und Löschen sammeln (`batch`).** `with db.batch(commit=False,
u_window=10_000) as b:` liefert ein `WriteBatch`; `b.put_cell(...)`,
`b.delete_cell(...)`, `b.delete_record(...)` und `b.delete_area(...)` nehmen
dieselben Argumente wie die Methoden der Datenbank (ohne `commit`) und
werden **sofort geprüft**. Der Endzustand ist derselbe wie mit den
Einzelaufrufen in derselben Reihenfolge: Einzelzell-Aufrufe zwischen zwei
Datensatz-/Bereichslöschungen werden je Zelle zusammengefasst (der letzte
`put_cell` gewinnt, `put_cell` → `delete_cell` löscht, auch eine
Null-Zelle); `delete_record`/`delete_area` wirken auf den Zustand nach den
Aufrufen davor (davor Geschriebenes wird gelöscht, danach Geschriebenes
bleibt; Wiederholungen zählen nichts doppelt). Beim Verlassen des Blocks
ohne Ausnahme wird alles als eine Einheit geschrieben (Einzelzellen je Realm
und Lane mit einem `put_points`; aufeinanderfolgende Datensatzlöschungen
teilen sich einen Tombstone-Puffer), `commit=True` committet danach. Innerhalb des Blocks gibt es **kein
read-your-writes**; endet der Block mit einer Ausnahme, wird nichts
geschrieben. Schlägt das Schreiben fehl (Größengrenze, E/A-Fehler, Realm
durch `rollback()` im Block entfernt), wird der ganze Batch zurückgenommen
– über alle Realm/Lane-Gruppen und `u`-Fenster: die Datei wird auf ihre
Länge vor dem Batch gekürzt und der Zustand des Handles neu aufgebaut
(einmaliges Neueinlesen wie beim Öffnen); frühere unkommittierte
Schreibvorgänge des Handles bleiben. `flush()` schreibt vorzeitig (`{"cells", "tombstones",
"deleted", "blocks", "bytes"}`; `deleted` = gelöschte lebende Zellen aller
Löschaufträge, wie die Summe der Rückgaben der Einzelaufrufe),
`discard()` verwirft, `len(b)` zählt die Aufrufe seit dem letzten `flush()`.
Ein fehlgeschlagener `flush()` (Schreib- oder Commit-Fehler, Handle von einem
anderen Thread belegt) **verbraucht den Batch**: nichts davon ist
geschrieben, jeder weitere Aufruf gibt `RuntimeError`, das Verlassen des
Blocks schreibt und committet nichts. Ein erfolgreicher `flush()` im Block
bleibt geschrieben, auch wenn der Block danach mit einer Ausnahme endet.
Ein Batch hält jeden Auftrag bis `flush()` im RAM (etwa 105 Byte je
Zellauftrag, etwa 86 Byte je Record-Auftrag). Bei Millionen von Aufträgen
zwischendurch `b.flush()` aufrufen; große zusammenhängende Bereiche direkt
mit `db.delete_area(...)` löschen, das in Paketen zu 8 192 Zellen arbeitet
und nur einen Bruchteil des Speichers braucht. Wirkung:
20 000 Einzelzellen ≈ 1,8× schneller, Datei ≈ 5× kleiner.

**Koordinaten, Realm, Lane, IDs:** `int`, Objekte mit `__index__` (z. B.
NumPy-Ganzzahlen) und Dezimal-Strings werden exakt übernommen; Floats,
`Decimal` und `Fraction` nur mit ganzzahligem Wert (`3.0`). `2.7` löst
`TypeError` aus (keine stille Kürzung wie `int()` in der Referenz).
Verwaltungs-IDs (`shape_id`, `layout_id`, `index_id`) sind auf 0 … 2^63 − 1
begrenzt (`ValueError`, eine negative ID mit „must be >= 0“; bei Parametern mit 64-Bit-Typ kann die Umwandlung
schon vorher `OverflowError` melden). Verschachtelte Werte (Listen/Dicts)
sind auf 256 Ebenen begrenzt (`ValueError`, auch bei zyklischen
Strukturen).

**Typprüfungen beim Schreiben:** Ganzzahlen, die nicht in ein endliches
`f32`/`f64` passen ⇒ `ValueError`; `bytes` für ein `utf8`-Feld ⇒ `TypeError`;
unbekannte Schlüssel in Dict-Zeilen ⇒ `SchemaError`; Schlüssel aus der
Ausgabe von `get` (Metadaten mit `_`, Extension-Namen und `v<n>`) werden von
`put` ignoriert, sodass `put(realm, u, db.get(realm, u))` funktioniert; `bytes(n)` aus einer
Ganzzahl höchstens 1 GiB.

**Schema-IDs:** `create_realm`, `create_shape`, `create_index` (und die
Layout-Anlage) mit einer bereits belegten ID lösen `SchemaError` aus, statt
die Definition zu ersetzen; die Default-Shape eines Realms ändert
`set_realm_default_shape`.

### 16.5 NumPy-Batch-API

Arrays werden über das Buffer-Protokoll übernommen (kontrollierte Kopie,
beliebige Endianness und Strides; kein `numpy`-Crate). Kodier-, Validier-
und Schreibschleifen laufen in Rust ohne GIL. `read_numpy` und `sql_numpy`
geben ihre Ergebnisvektoren ohne Kopie an NumPy weiter (beschreibbare
Arrays über dem Puffer `ColumnBuffer`, der so lange lebt wie das Array);
`read_numpy` dekodiert numerische BASE-Werte von Records ohne Sparse-Overlay
und ohne Extensions direkt in die typisierte Spalte (1 Mio × 11 Felder:
3,9 → 6,5 Mio Records/s, Spitzen-RSS 309 → 240 MiB).

| Methode | Beschreibung |
|---|---|
| `put_numpy_batch(realm_id, u_start, columns, *, shape_id=None, lane=None, commit=False)` | CLASSIC/BASE-Batch ab `u_start` |
| `put_objects_numpy(layout_id, object_ids, columns, *, lane=None, commit=False, u_window=10000)` | beliebiges Layout |
| `put_cells_numpy(realm_id, u, v, values, *, type=None, lane=None, commit=False, u_window=10000)` | Sparse-Zellen |
| `read_numpy(realm_id, u_start, count, fields=None, *, lane=None, object_fallback=False, masks=False)` | Spalten als Arrays + `_present` (+ `_cells` mit `masks=True`) |

Spalten sind eindimensionale Arrays, Buffer-Objekte (`memoryview`,
`array.array`) oder Python-Sequenzen. `ndim != 1` ⇒ `ValueError`; `str`,
`bytes`, `bytearray`, `dict` als Spalte ⇒ `TypeError`; alle Spalten müssen
gleich lang sein (`ValueError`, auch bei leerer erster Spalte). Unterstützte
dtypes: bool, int8 … int64, uint8 … uint64, float32, float64, object,
str/bytes; float16, longdouble, complex, datetime/timedelta und strukturierte
dtypes ⇒ `TypeError`. Skalare (Python-`int`, NumPy-Skalar, 0-d-Array) für
`u`/`v`/`object_ids` werden auf alle Zeilen übertragen.

| Feldtyp | akzeptierte Spalten | abgewiesen (`TypeError`) |
|---|---|---|
| `u8 … i64`, `varuint`, `varint` | Integer-Arrays jeder Breite (Bereich wird geprüft), Listen, BigInt-Objekte | bool-, Float-Arrays |
| `f32`, `f64` | Float- und Integer-Arrays; NaN/±Inf sind gültige Werte (wie Einzel-API); endliche Werte außerhalb `f32` ⇒ `ValueError` | bool-Arrays |
| `bool` | Bool- und Integer-Arrays | Float-Arrays |
| `utf8`, `bytes` | Objekt-, str-, bytes-Arrays oder Listen | Integer-, Float-, bool-Arrays |
| Enums | Objekt-Arrays oder Listen (Labels), Integer-Arrays (Codes) | bool-, Float-Arrays |

`read_numpy` liefert genau `count` Zeilen; der dtype hängt nur vom Feldtyp ab:
`varuint` `uint64`, Integer-Typen `int64`, `float32/64`, `bool`, sonst
Objekt-Arrays. Passt ein gespeicherter Wert nicht (z. B. `varuint` > 2^64 − 1),
gibt es `ValueError`; mit `object_fallback=True` wird diese Spalte ein
Objekt-Array. `_present` folgt der Existenzregel (mindestens eine lebende
Zelle; reine Sparse-Records sind vorhanden und lesen ihre Einzelzellen, ein
Record mit lauter gelöschten Zellen nicht). Fehlende Werte: 0/`False`, NaN
bzw. `None`. `masks=True` liefert zusätzlich `"_cells"` (Feldname →
bool-Array: Zelle lebt, gespeichertes `None` eingeschlossen) und macht echte
`0`/`False` von fehlenden Zellen unterscheidbar.
Ein `count`, dessen Ergebnis nicht in den physischen Speicher passt, löst
`MemoryError` aus (kein Prozessabbruch); negatives `count` ⇒ `ValueError`. **Große Koordinaten:** Skalare wie
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
│   ├── MathLayoutError
│   │   ├── MathLayoutNotFound
│   │   ├── MappingFrozenError
│   │   └── MathCellConflictError
│   ├── MathError
│   │   ├── MathSyntaxError
│   │   └── MathDomainError
│   ├── LayoutError              (Fehler des MATH-Realmplaners)
│   │   ├── LayoutStateError
│   │   ├── RealmBudgetExceeded
│   │   ├── StalePlanError
│   │   └── AllocationConflictError
│   └── QueryError
│       ├── QueryPlanError
│       ├── QueryDomainError
│       └── SQLSyntaxError
└── ValueError
    └── IndexConfigError
```

`MathError`, `LayoutError` und `QueryError` sind seit 0.28.0
Unterklassen von `NHyDBError`; `except nhydb.NHyDBError` fängt damit alle
bibliotheksspezifischen Fehler außer `IndexConfigError` (ein `ValueError`).
Definiert ist die Hierarchie in `crates/nhydb-py/src/exceptions.rs`.

Daneben verwendet die Engine Builtins:

| Ausnahme | Anlass |
|---|---|
| `ValueError`, `TypeError`, `KeyError` | ungültige Argumente, Werte und Namen (z. B. nicht ganzzahlige Koordinaten ⇒ `TypeError`, IDs > 2^63 − 1 ⇒ `ValueError`, zu tief verschachtelte Werte ⇒ `ValueError`) |
| `OverflowError` | Python-Ganzzahl passt nicht in einen Parameter mit festem 64-Bit-Typ (z. B. `layout_id=2**64`) |
| `UnicodeEncodeError` | Text mit einzelnen Surrogaten (nicht als UTF-8 kodierbar) |
| `MemoryError` | echter Speichermangel bzw. Ergebnis größer als der physische Speicher (z. B. `read_numpy(count=…)`) |
| `OSError` und Unterklassen (`FileNotFoundError`, `FileExistsError`, `PermissionError`, `IsADirectoryError` …) | Dateisystemfehler, mit `errno` und – wo bekannt – `filename` |
| `RuntimeError` | Schreibaufruf auf einem Objekt, das ein anderer Thread gerade benutzt (oder umgekehrt), Rückruf eines Plugins in die gerade schreibende Datenbank (PyO3: „Already borrowed“), Benutzung eines beendeten `WriteBatch` |
| `RuntimeWarning` (Warnung) | Hinweise beim Öffnen (`db.open_warnings`) |

Beschädigte Dateien ergeben `CorruptionError`. Ein `IndexError` oder eine
`PanicException` aus Datei- oder Benutzereingaben ist ein Fehler der Engine.

**Threads.** Ohne GIL laufen: `query`, `explain_query`, `sql`, `sql_numpy`,
die `QueryEngine`-Methoden, `stats`, `health`, `verify`, `read_numpy`, die
Batch-Schreiber (`put_dense_batch`, `put_points`, `put_numpy_batch`,
`put_cells_numpy`, `put_objects_batch`, `put_objects_numpy`,
`put_math_batch`, `put_math_numpy`, `WriteBatch.flush`), `build_indexes`,
`commit`, `checkpoint`, `sync`, `rollback`, `close`, Öffnen/Anlegen sowie
`compact_file`, `repair_file`, `restore_backup` und `inspect_tail`
(`checkpoint`, `sync` und `inspect_tail` seit 0.28.2). Einzelschreibaufrufe mit
`commit=True` behalten den GIL auch für den Commit: Gemessen stieg
die Commitzeit eines `put_cell(commit=True)` sonst von 0,9 auf 7,7 ms, wenn
ein anderer Python-Thread rechnet (Rückgabe des GIL). Python-Laufzeit-Provider und
Layout-Plugins holen sich den GIL für ihren Rückruf zurück. `get`, `find_u`
und andere Aufrufe im µs-Bereich behalten den GIL **bewusst**: Gemessen
kostete die Übergabe je Aufruf 3–8 % im Einzelthread und halbierte
(`get`) bzw. fünftelte (`find_u`) den Durchsatz von vier Threads.

Ein Objekt wird von lesenden Aufrufen geteilt und von schreibenden exklusiv
belegt. Mehrere Threads dürfen gleichzeitig lesende Abfragen auf
**demselben** Objekt ausführen (getestet mit `sql`, `query`, `stats`,
`health`). Ein Schreibaufruf, `commit` oder `rollback`, während ein anderer
Thread das Objekt benutzt (oder umgekehrt), wird sofort mit `RuntimeError`
(„Already borrowed“ / „Already mutably borrowed“) abgewiesen – er wartet
nicht und verändert keine Daten (getestet: nach solchen Konflikten sind alle
erfolgreichen Schreibvorgänge sichtbar, `verify(deep=True)` ist sauber).
Für parallele Arbeit je Thread ein eigenes Objekt verwenden (lesende Threads
mit `readonly=True`, denn die Schreibsperre lässt nur ein schreibendes
Objekt je Datei zu; vier Threads mit eigenen Objekten: SQL-Scan über 1 Mio
Records ≈ 2,9× schneller als einer, Punktabfrage ≈ 1,65×) oder Schreib-
und Lesezugriffe mit einem `threading.Lock` serialisieren. Das gilt auch
für Iteratoren und `QueryEngine`-Objekte, die eine Datenbank benutzen.
Mehrere Leserprozesse mit `readonly=True` skalieren nahezu linear
(`get_value`, 1/2/4/6 Prozesse: 3,3/5,8/10,5/13,0 Mio Aufrufe/s
aggregiert).

### 16.8 CLI

Das Paket installiert den Befehl `nhydb` (gleichwertig: `python -m nhydb`);
`nhydb --help` bzw. `nhydb <befehl> --help` zeigen die Optionen.

| Befehl | Zweck |
|---|---|
| `to-n`, `from-n` | Adressumrechnung ohne Datenbank |
| `layout`/`get-layout`, `layout-check`, `layout-plan`, `evolve-layout`, `route`, `logical-address` | Layouts |
| `describe`, `occupied`, `get`/`get-n`, `address`, `cell`, `cell-n`, `find` | Lesen |
| `stats`/`info`, `health`, `verify`, `rebuild-indexes`, `repair`, `restore`, `compact` | Betrieb |

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

* Lesebefehle öffnen die Datenbank mit `readonly=True` (die Datei wird nie
  verändert); `verify` funktioniert auch bei beschädigten Dateien (Rückfall
  auf `inspect_tail`).
* Exitcodes: `0` Erfolg, `1` eine Prüfung hat Probleme gefunden (`verify`,
  `health` mit `unhealthy`, `repair`-Probelauf mit Befund), `2`
  Bedienfehler oder abgelehnte Operation, `3` Fehler. Fehler erscheinen als
  Meldung auf stderr, ohne Traceback.
* `repair` ist ohne `--apply` ein Probelauf; `--allow-data-loss` und
  `--keep-uncommitted` entsprechen den Schaltern von `repair_file`.
  `restore BACKUP DB` spielt eine Sicherung wieder ein.
* In JSON-Ausgaben erscheinen `bytes` als `{"$bytes_b64": "…"}`.
* Zahlen mit mehr als 4 300 Dezimalstellen kann Python standardmäßig nicht
  als Text ausgeben (Abschnitt 3.6); dafür die Umgebungsvariable
  `PYTHONINTMAXSTRDIGITS=0` setzen.

### 16.9 Vollständiges Beispiel

Das folgende Beispiel wurde mit NHyDB 0.28.0 ausgeführt; die
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
| Python | `requires-python = ">=3.12"` (`pyproject.toml`) | für das Python-Paket `nhydb` |
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

**Windows-Pfade:** In TOML leitet ein Backslash in doppelten Anführungszeichen
eine Escape-Sequenz ein – `"C:\daten"` wird abgelehnt, aus `"C:\neu"` wird
stillschweigend ein Zeilenumbruch. Windows-Pfade daher in einfache
Anführungszeichen setzen (`temp_dir = 'C:\daten\nhydb'`) oder mit `/`
schreiben (`temp_dir = "C:/daten/nhydb"`).

```toml
[database]
db_max_size_bytes = 53687091200   # 50 GiB; Startwert für NEUE Datenbanken (mind. 65536)
durability = "fsync"              # "fsync" | "full"

[math]
realm_auto_limit = 4096           # Startwert für NEUE MATH-Layouts
math_max_intermediate_bits = 1048576   # wird je MATH-Layout beim Anlegen eingefroren

[lock]
enabled = true                    # Sperrdatei <db>.lock
timeout_ms = 0                    # 0 = sofort Fehler, sonst Wartezeit in ms

[paths]
# layout_path = "{db}.layout.enc"           # {db} = Dateiname der Datenbank
# key_path    = "/sicherer/ort/{db}.{db_path_hash}.layout.key"

[cache]
total_bytes = 4294967296          # Prozessbudget aller Caches (4 GiB; 0 = sofort verdrängen)
block_cache_bytes = 67108864      # Block-Cache (64 MiB)
posting_cache_bytes = 268435456   # dekodierte Postings und Dokumentlängen (256 MiB)
sparse_cache_bytes = 134217728    # dekodierte Sparse-Daten (128 MiB)
vindex_max_marks = 4000000        # Budget der gecachten v-Indizes (× 48 Byte ≈ 192 MB)
vindex_max_indexes = 4096         # Anzahl gecachter v-Indizes
index_page_cache_bytes = 33554432 # dekodierte Seiten der Index-Schlüsselbäume (32 MiB)

[index]
build_memory_bytes = 536870912    # Speicherbudget von build_indexes() (512 MiB), darüber Auslagern

[write]
max_block_bytes = 8388608         # größter DATA-Block eines dichten Schreibaufrufs (8 MiB; 0 = ein Block je Aufruf, sonst mindestens 64 KiB)

[directory]
flush_blocks = 65536              # Daten-/DELETE-Blöcke seit dem letzten Verzeichnislauf
flush_bytes = 268435456           # Payloadbytes seit dem letzten Verzeichnislauf (256 MiB)
cache_entries = 262144            # dekodierte Verzeichniseinträge im Speicher

[query]
max_result_rows = 0               # optionale Grenze der geschätzten Ergebniszeilen (0 = keine)
max_result_bytes = 4294967296     # Speichergrenze von sql()/execute()/search()/sql_numpy() (4 GiB, 0 = keine)
overlay_view = true               # schnelle Wege auch auf überschriebenen/gelöschten/Sparse-Daten (Gewinner-Sicht; false = allgemeiner Weg)
overlay_view_max_bytes = 268435456  # Speichergrenze einer Gewinner-Sicht (256 MiB); größer = allgemeiner Weg

[results]
# temp_dir = "/schnelle/ssd/nhydb-results"   # Standard <System-Temp>/nhydb-results
budget_bytes = 0                  # Bytes verwalteter Ergebnisdateien je Prozess (0 = kein Budget)
compress = false                  # zlib-komprimierte Chunks als Standard
sort_memory_bytes = 268435456     # Sortierschlüssel im Speicher vor Läufen (256 MiB, mindestens 64 KiB)

[limiter]
enabled = true                    # false = nur messen und zählen, nie steuern
reserve_percent = 10              # freizuhaltender Anteil des RAM
warn_percent = 5                  # Breite der gelben Zone in % des RAM
tick_ms = 20                      # Takt des Überwachungsthreads
notify_wait_s = 300               # ab dieser Wartezeit "waiting_long"
wait_timeout_s = 0                # 0 = Warten führt nie zum Abbruch
cancel_over_limit = true          # letzter Ausweg: Abbruch des einzigen weiter wachsenden Vorgangs über der Grenze
probe_bytes = 268435456           # TEST_IT-Probe (256 MiB, 0 = aus)
ram_bytes = 0                     # angenommener RAM (0 = physischer RAM; mindestens 64 MiB)
dry_run = false                   # Diagnose: Entscheidungen nur zählen

[limiter_log]
enabled = true                    # Betriebsstatistik limiter.txt
# path = "/var/log/nhydb"         # Verzeichnis oder Datei *.txt; Standard: Verzeichnis der Datenbank
interval_min = 30                 # ein Datensatz je Abschnitt
max_bytes = 60000000              # Rotationsschwelle (60 MB): danach ZIP-Archiv und neue Datei
buffer_records = 16               # Datensätze im Speicher bis zum Schreiben
```

Regeln:

* Explizite API-Parameter (`layout_path`, `key_path`, `durability`, `lock`,
  `lock_timeout_ms`) haben Vorrang vor der Datei.
* In der Datenbank persistierte Werte (Größengrenze im `DB_CONFIG`-Block,
  `realm_auto_limit` je MATH-Layout) werden beim Öffnen **nie** überschrieben.
* Unbekannte Schlüssel sind Fehler; `realm_span`, `nl`, `nr` sind verbotene
  Schlüssel (feste Systemvorgaben).
* Relative Pfadvorlagen beziehen sich auf das Verzeichnis der Datenbank.
  Platzhalter: `{db}` = Dateiname der Datenbank, `{db_path_hash}` = die
  ersten 16 Hexziffern des SHA-256 des absoluten DB-Pfads (eindeutig je
  Speicherort; für mehrere gleichnamige Datenbanken mit gemeinsamem
  Schlüsselverzeichnis).
* `durability` akzeptiert auch die Aliasse `default`/`normal` (= `fsync`)
  und `fullfsync` (= `full`).
* `[lock] enabled`/`timeout_ms` und die API-Parameter `lock`/
  `lock_timeout_ms` werden feldweise kombiniert.
* `db_max_size_bytes` muss mindestens 65 536 (64 KiB) betragen.
* `math_max_intermediate_bits` wird je MATH-Layout beim Anlegen eingefroren
  (Abschnitt 13.2); spätere Änderungen der Datei wirken nur auf neue
  MATH-Layouts.
* **Abschnitt `[cache]`:** `block_cache_bytes` bestimmt das Budget des
  Block-Caches (0 = kein Cache, nur Teil-Lesen geprüfter Blöcke).
  `vindex_max_marks` begrenzt seit 0.28.2 die gecachten Sparse-v-Indizes (je
  gelesener u-Gruppe einer) in Bytes, gemessen in Einheiten einer
  Sprungmarke zu 48 Byte (Standard 4 000 000 ≈ 192 MB);
  `vindex_max_indexes` begrenzt ihre Anzahl (4 096). Verdrängt wird nach LRU;
  ein Index, der größer als das ganze Budget ist, wird allein gehalten
  (Abschnitte 9.4 und 9.8). `index_page_cache_bytes` (ab 0.28.2) begrenzt
  die dekodierten Seiten der SECONDARY-Schlüsselbäume (Standard 32 MiB, LRU,
  Abschnitte 7.8 und 9.8); der Speicher der Indizes wächst damit beim Lesen
  nicht mehr mit ihrer Schlüsselzahl.
* **Prozessbudget und Pools** (ab 0.28.2, Abschnitt 9.8): `total_bytes`
  ist das Budget aller Caches aller Objekte eines Prozesses;
  `posting_cache_bytes` und `sparse_cache_bytes` begrenzen die dekodierten
  Index-Payloads bzw. Sparse-Daten je Objekt.
* **Abschnitte `[query]` und `[results]`** (ab 0.28.2, Abschnitt 14.5):
  Grenze je Abfrage, Ergebnisverzeichnis, Budget, Kompression und
  Sortierspeicher der externen Sortierung.
* **Abschnitt `[index]`** (ab 0.28.2, Abschnitt 12.2): `build_memory_bytes`
  ist der geschätzte Speicher von `build_indexes()` für die gesammelten
  Schlüssel und Postinglisten (Standard 512 MiB); darüber werden sie als
  sortierte Läufe in eine anonyme temporäre Datei geschrieben und beim
  Schreiben der Schlüsselbäume gemischt.
* **Abschnitt `[write]`** (ab 0.28.2, Abschnitt 7.6): `max_block_bytes`
  begrenzt die Payload eines DATA-Blocks, den ein dichter Schreibaufruf
  erzeugt (Standard 8 MiB = kleinster größter cachebarer Eintrag des
  Block-Caches). Größere Aufrufe werden in aufeinanderfolgende Blöcke
  geteilt, atomar wie ein Block. 0 = ein Block je Aufruf (Verhalten von
  0.28.0), sonst mindestens 64 KiB. Wirkt auch beim Umschreiben durch
  `compact_file`, das ältere große Blöcke so aufteilt.
* **Abschnitt `[directory]`** (ab 0.28.2, Abschnitt 7.10): Ein
  Verzeichnislauf wird geschrieben, sobald seit dem vorigen
  `flush_blocks` Daten-/DELETE-Blöcke oder `flush_bytes` Payloadbytes
  hinzugekommen sind (nach einem Schreibaufruf oder im Commit);
  `cache_entries` begrenzt die dekodierten Verzeichniseinträge im Speicher
  (LRU, ≈ 130 Byte je Eintrag, mindestens 128). `stats()["directory"]`
  zeigt den Zustand.

### 17.3 Installation und Build aus dem Quellcode

**Installation von PyPI** (vorgefertigte Wheels für CPython 3.12–3.15 unter
Linux x86_64/aarch64, macOS x86_64/arm64 und Windows x64):

```bash
pip install nhydb
pip install numpy                             # optional, für die NumPy-API
```

**Build aus dem Quellcode** (Rust-Toolchain erforderlich, siehe `BUILDING.md`):

```bash
python3 -m venv .venv
source .venv/bin/activate            # Windows: .venv\Scripts\activate
pip install maturin
maturin build --release --out dist   # Wheel unter dist/
pip install dist/nhydb-0.28.2-*.whl
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
UTF-8-JSON) und Layoutplan sind plattformneutral. Die CI baut und testet auf
Linux, macOS und Windows (`.github/workflows/ci.yml`); Einzelheiten zu
Durability, Sperre und Schlüsseldateien je Plattform stehen in
`docs/PLATFORM.md`.

### 17.5 Betriebshinweise

* **Sicherung:** Immer alle drei Dateien sichern (`.nhydb`, `.layout.enc`,
  `.layout.key`); die Schlüsseldatei getrennt und zugriffsgeschützt.
* **Umbenennen:** Eine umbenannte DB-Datei öffnet nicht (R0,v2). Für eine
  Kopie unter neuem Namen `compact_file(path, output=neu)` verwenden.
* **Mehrere Prozesse und Threads:** genau ein schreibendes Objekt je
  Datenbank (die Sperre wird bei jedem schreibenden Öffnen exklusiv
  genommen); `lock_timeout_ms` wartet. Weitere Leser öffnen mit
  `readonly=True` (ohne Sperre, Stand zum Zeitpunkt des Öffnens; für einen
  neueren Stand neu öffnen, ≈ 1 ms bei 1 Mio Records). Auf einem Objekt
  nicht gleichzeitig schreiben und lesen; parallele Threads mit eigenen
  Objekten (Abschnitt 16.7).
* **Caches:** `[cache] block_cache_bytes` an die Arbeitsmenge anpassen
  (Abschnitt 9.8); `stats()["block_cache"]` zeigt Treffer, Ladevorgänge,
  Teil-Lesungen und Verdrängungen sowie die Belegung des v-Index-Caches
  (`vindex_bytes`, `vindex_budget_bytes`). Viele einzeln geschriebene
  Sparse-Zellen eines `u` (nicht konsolidierte Anker) kosten im v-Index
  ≈ 60 Byte je lebendem Einzelzellen-Record; Kompaktierung oder
  Batch-Schreiben (`db.batch()`, `put_points`) bündelt sie.
  `[cache] index_page_cache_bytes` begrenzt die Seiten der Indizes
  (`index_page_bytes`, `index_page_evictions`), `[index]
  build_memory_bytes` den Indexaufbau (Ergebnis `spill_rounds`,
  `spill_bytes`; Abschnitt 12.2).
* **Blockverzeichnis:** `stats()["directory"]` zeigt Läufe, Stufen,
  Tail-Größe und Fehlerzähler; `degraded` gesetzt ⇒ `repair_file` verwirft
  das Verzeichnis verlustfrei (Abschnitte 7.10 und 15.4).
* **Schlüsseldatei unter Windows:** erbt die ACL ihres Verzeichnisses; in
  ein nur für den Benutzer zugängliches Verzeichnis legen (Abschnitt 8.3).
* **Durability:** `fsync` ist Standard (unter macOS mit Schreibbarriere vor dem
  Superblock, Abschnitt 9.6); für Schutz gegen Laufwerkscache-Verlust
  auf macOS `durability="full"` wählen (Commit ≈ 3,6 ms, im Kernbenchmark
  vor der Optimierung rund 30–60 % geringerer Schreibdurchsatz,
  Abschnitte 9.6 und 18.5).
* **Viele Einzelzellen:** `db.batch()` (Abschnitt 16.4) statt vieler
  einzelner `put_cell`-Aufrufe; ungewollte, noch nicht committete Änderungen
  mit `rollback()` verwerfen.
* **Wartung:** regelmäßig `verify()`/`health()`; vor `repair_file(apply=True)`
  und Kompaktierung Kopien sichern (Backups von DB und Plan sind
  standardmäßig aktiv; Wiederherstellung mit `NHyDB.restore_backup`). Die
  Kompaktierung braucht begrenzten Arbeitsspeicher (Batch `batch_records`,
  speicherbegrenzte Sparse-Zusammenführung), aber Plattenplatz für die neue
  Datei, die Sicherung (vorab geprüft) und ggf. temporäre Läufe
  (Abschnitt 15.5).
* **Aktualisierung von 0.28.0:** Dateien im Format 0.16
  (`NHYDB016`) werden von 0.28.2 nicht gelesen (keine
  Rückwärtskompatibilität in der Entwicklungsphase): `CorruptionError` mit
  Nennung des Formats, die Datei bleibt unverändert. Einen Migrationsweg
  gibt es nicht; die Datenbank aus den Quelldaten bzw. Exporten mit 0.28.2
  neu aufbauen und die Indizes mit `build_indexes()` anlegen (Abschnitt 7).
* **Aktualisierung von 0.27.1:** Datenbanken von NHyDB 0.27.1 oder älter
  öffnen mit 0.28.x nicht; die Daten mit 0.27.1 exportieren, mit 0.28.2 in
  eine neu angelegte Datenbank importieren und die Indizes mit
  `build_indexes()` aufbauen (Abschnitte 7 und 12.2).

---

## 18. Tests, Validierung und Benchmarks

### 18.1 Teststrategie

1. **Unit- und Integrationstests im Kern** (`cargo test --workspace --release`):
   Unit-Tests in `crates/nhydb-core/src/**` sowie die Integrationstests
   `crates/nhydb-core/tests/selftest.rs`,
   `crates/nhydb-core/tests/sparse_access.rs` und (seit 0.28.2)
   `crates/nhydb-core/tests/memory_bounds.rs`.
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
Unter CPython 3.12, 3.13 und 3.15 (Unicode 15.0, 15.1 und 17.0) werden deshalb die beiden
vollständigen Vergleiche gegen das laufende `unicodedata` übersprungen
(71 bestanden, 2 übersprungen); die festen Unicode-16.0.0-Referenzwerte
werden auf jeder Version geprüft. Unter Windows wird
`test_failed_append_is_rolled_back` übersprungen, weil er die
POSIX-Dateigrößengrenze (`RLIMIT_FSIZE`) benötigt; dieselbe Eigenschaft
(kein Teilblock nach einem Schreibfehler) prüft auf allen Plattformen der
Rust-Test `db::file_handle_tests::failed_append_is_rolled_back`. `test_kernel.py` überspringt den AES-GCM-Vergleich,
wenn das Paket `cryptography` fehlt; `test_sql.py` überspringt die
NumPy-Ausgabe ohne `numpy`.

Die Zahlen oben beschreiben den Release-Stand 0.27.1. Mit der Stabilisierung
in 0.28.0 (`CHANGELOG.md`) kamen die Regressionstests
`tests/python/test_stab_*.py` hinzu: Persistenz, Öffnen, Repair und Restore
(`test_stab_persistence.py`), Schema-Digest und Ablehnung von
0.27.1-Datenbanken (`test_stab_schema_digest.py`), CLI (`test_stab_cli.py`), große Zahlen
(`test_stab_bigint.py`), Codec und Eingabeumwandlung (`test_stab_codec.py`),
ID-Grenzen (`test_stab_ids.py`), MATH-Grenzen (`test_stab_math.py`),
Index gegen Scan als Matrix aus Szenarien, Prädikaten und Rückgabemodi
(`test_stab_index_vs_scan.py`), Query-API und geschlossene Datenbank
(`test_stab_query_api.py`), Rekursionsgrenzen im Subprozess
(`test_stab_recursion_limits.py`) sowie Python- und NumPy-API einschließlich
Threads (`test_stab_api.py`); dazu weitere Rust-Unit-Tests (u. a.
Fehlerinjektion für fsync und Kompaktierung).

Die Leistungsüberarbeitung von 0.28.0 ergänzt Ergebnisgleichheits- und Invariantentests:
Lesepfad und Block-Cache bei Budgets 0 bis 256 MiB inkl. Korruption
(`test_perf_lesen.py`), Schnellpfade der Abfragen gegen den allgemeinen Pfad
(`test_perf_query_paths.py`), Kompaktierung, `put_math`, GIL-Freigabe,
NumPy-Ausgabe, `batch()` und `rollback()` (`test_perf_schreiben.py`) sowie
Cache-Kohärenz nach `rollback`, `batch` und Kompaktierung
(`test_perf_integration.py`); Rust-Tests u. a. in `db/blockcache.rs`.

Mit 0.28.2 kamen hinzu: Blockverzeichnis über die Python-API mit sehr
kleinen Schwellen, beschädigte Verzeichnisblöcke und speicherbegrenzte
Kompaktierung (`test_stab_directory.py`), Golden-Test der Blockpayloads
(`test_stab_format_golden.py`), der Regressionspool zu 0.28.2
(`test_regress_0282.py`); in Rust Zwillingsvergleiche mit und ohne
Verzeichnis (`db/directory_tests.rs`), Komplexitätstests über gezählte
Lesezugriffe (`db/complexity_tests.rs`) und Speichergrenzen mit zählendem
Allokator (`crates/nhydb-core/tests/memory_bounds.rs`), Unit-Tests der
Schlüsselbäume (`db/secindex.rs`: Schlüsselkodierung und Ordnung von
Schreiber und Leser, tiefe Bäume gegen eine Brute-Force-Suche).

### 18.3 Versionskennungen

| Kennung | Wert | Bedeutung |
|---|---|---|
| Produktversion | `0.28.2` | identisch in Cargo-Workspace (`nhydb-core`, `nhydb-py`), Python-Paket `nhydb` (Version über maturin aus Cargo) und `nhydb.ENGINE_VERSION` |
| Dateiformat | `FILE_FORMAT_VERSION = "0.28.2"`, Signatur `NHYDB` + Bytes `00 1C 02`, Superblock Major 0 / Minor 28, Blockversion 16 | die Formatversion ist das Engine-Release, das das Format festgelegt hat; sie ändert sich nur bei einer Änderung des Formats auf dem Datenträger. Nur dieses Format wird gelesen; das Format 0.16 (`NHYDB016`) von NHyDB 0.28.0 und 0.27.1 sowie ein unveröffentlichtes Entwicklungsformat (`NHYDB017`) ergeben `CorruptionError` mit Nennung des Formats (Abschnitt 7) |
| `QUERY_VERSION` | `"0.27"` | Kennung der Abfrageschicht; erscheint als `query_version` in Abfrageergebnissen; keine Produktversion |
| `SQL_LANGUAGE_VERSION` | `"NHySQL-0.24"` | Kennung des Sprachumfangs von NHySQL; erscheint als `language` in SQL-Ergebnissen; keine Produktversion |
| Contract-Kennungen | `NHYDB027_R0U1_V1`, `NHYDB027_KEY_V1`, `NHYDB027_EXTERNAL_LAYOUT_V1`, `NHYDB027_SCHEMA_V1`, `NHYDB_DB_CONFIG_V015`, MAC-Domäne `NHYDB027-LAYOUT-MAC:`, Schema-Digest-Domäne `NHYDB027-SCHEMA-DIGEST:` | feste Bezeichner der JSON-Strukturen bzw. der Schlüsselableitung im Dateiformat (Abschnitte 7.5 und 8.3); die enthaltenen Ziffern sind Teil des Bezeichners, keine Produktversion |

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

#### Leistungsüberarbeitung (0.28.0)

Die Skripte `benchmarks/bench_028_lesen.py`, `bench_028_query.py` und
`bench_028_schreiben.py` (Beschreibung, `--scale` und Lastlauf für größere
Rechner: `benchmarks/README.md`) vergleichen zwei Builds abwechselnd.
Gemessen am 2026-10-20 auf demselben MacBook Air (M2, 16 GB), Release-Builds,
seriell ohne andere Last, Median aus fünf abwechselnden Läufen (Lesepfad:
drei Läufe zu je fünf Wiederholungen) gegen den Stand vor der Überarbeitung.
Alle Ergebnisse waren in beiden Ständen identisch.

| Fall (Skala 1) | vorher | nachher |
|---|---:|---:|
| `get` in einem Block mit 1 Mio Records (nach erster Prüfung) | 4 035 µs | 3,0 µs |
| `get_value` 4 000 zufällige Records über ≈ 100 MB, warm, 64 MiB Cache | 20,5 µs | 2,1 µs |
| Öffnen, 1 Mio Records in 20 000 Blöcken | 24,5 ms | 4,8 ms |
| `get_value` (Dense, ohne Overlay) | 1,36 Mio/s | 4,10 Mio/s |
| `put_cell`/`get_value` im Wechsel | 8 210 Paare/s | 304 800 Paare/s |
| `range` über 80 000 Einzelsegmente | 3 786 ms | 172 ms |
| `Q.u.between(1, 4097)` über 1 Mio Records | 271 ms | 10,2 ms |
| SQL `COUNT(*)` (1 Mio), erster Aufruf / wiederholt | 764 / 750 ms | 1,9 / 0,03 ms |
| SQL `ORDER BY id DESC LIMIT 5` / `GROUP BY` (1 Mio) | 2 479 / 2 048 ms | 578 / 541 ms |
| Kompaktierung 1 Mio Records + Overlays (Zeit / Spitzen-RSS) | 3,38 s / 1 880 MiB | 1,13 s / 67 MiB |
| Kompaktierung mit 100 000 Tombstones (Zeit / Spitzen-RSS) | 6,50 s / 1 970 MiB | 3,72 s / 145 MiB |
| `put_math`, 2 000 Realms | 882/s | 62 060/s |
| Commit `fsync` / `full` (macOS, 100 Records) | 0,19 / 4,99 ms | 0,19 / 3,95 ms |
| `read_numpy` 1 Mio × 11 Felder | 3,99 Mio Rec/s | 6,61 Mio Rec/s |
| `put_cell` einzeln, Commit je 1 000 (nachher auch mit `batch()`) | 397 000/s | 402 000/s (`batch()`: 578 000/s) |
| SQL-Scan, 4 Threads mit eigenen `readonly`-Handles (Beschleunigung) | 0,99× | 3,35× |

`COUNT(*)` prüft beim ersten Aufruf je Handle die Prüfsummen aller
beteiligten DATA-Blöcke; Wiederholungen nutzen das Ergebnis dieser Prüfung.

#### Sekundärindizes als Schlüsselbäume (0.28.2)

Gemessen auf einem Apple M1 Max, Release-Build, 1 Mio Zeilen mit vier
Indizes (Felder `k`, `s`, `flag` und ein TOKEN-Index `token`), Seiten-Cache
4 MiB, sofern nicht anders angegeben; Vergleich mit dem vorherigen
Indexformat 0.16 (Indexverzeichnis je SECONDARY-Block). Messwerte,
keine Zusagen.

| Fall | vorher | nachher |
|---|---:|---:|
| erste Punktabfrage nach dem Öffnen | 200 ms | 0,14–0,2 ms |
| Punktabfrage warm (Seite im Cache) | 0,54 µs | 0,41 µs |
| Punktabfrage kalt (Seite nicht im Cache) | – | ≈ 11 µs |
| RSS nach ≈ 15 000 gemischten Abfragen | 722 MB | 75–90 MB |
| Indexbytes auf der Platte | 92 MB | ≈ 16,8 MB |
| `build_indexes` | 8,0 s | 2,1–2,6 s |
| Öffnen | 0,28 ms | 0,26 ms |

Exakter Heap mit zählendem Allokator (Aufbau-Budget 8 MiB, Seiten-Cache
2 MiB): Spitze beim Aufbau 10,3 MB bei 200 000 Schlüsseln bzw. 12,6 MB bei
800 000; erste Abfrage 0,02 MB; lebender Heap nach 40 000 Abfragen 5,3 MB
bei beiden Größen. Die in Abschnitt 9.8 genannten nicht budgetierten
Strukturen (u. a. Posting-Liste eines sehr häufigen Schlüssels,
FULLTEXT-Korpus) kommen je nach Daten hinzu.

## 19. Bekannte Einschränkungen

### 19.1 Einschränkungen

| Thema | Stand |
|---|---|
| Nutzdatenverschlüsselung | Nur der Layoutplan ist verschlüsselt; Daten in `X.nhydb` liegen im Klartext |
| Schlüsseldatei | ohne die passende `*.layout.key` lässt sich die Datenbank nicht öffnen (fail closed); ein verlorener Schlüssel kann nicht wiederhergestellt werden |
| Schlüsselmaterial im Speicher | Schlüsselobjekt und abgeleitete Schlüssel werden genullt; interne Zustände der HMAC-/AES-GCM-Implementierungen und temporäre Puffer beim Laden der Schlüsseldatei nicht garantiert (Abschnitt 15.6); NHyDB ist kein Sicherheitsprodukt |
| Nebenläufigkeit | ein schreibendes Objekt je Datenbank (exklusive Sperre `<db>.lock`, auch innerhalb eines Prozesses); weitere Leser mit `readonly=True` (ohne Sperre, Stand zum Zeitpunkt des Öffnens, kein Nachladen); keine Mehrprozess-Transaktionen |
| Threads | lesende Aufrufe dürfen ein Objekt teilen; ein Schreibaufruf neben einem anderen Aufruf auf demselben Objekt ⇒ sofortiger `RuntimeError`, keine Datenänderung; `get`/`find_u` behalten den GIL (Abschnitt 16.7) |
| Transaktionen | Atomarität über die Commit-Grenze; `rollback()` verwirft alle Änderungen des Objekts seit seinem letzten Commit; keine verschachtelten Transaktionen/Savepoints; Mehrblock-Operationen sind seit 0.28.2 je Aufruf atomar (Schreibklammer, Abschnitt 9.3) |
| `batch()` | kein read-your-writes innerhalb des Blocks; sammelt `put_cell`, `delete_cell`, `delete_record`, `delete_area` |
| Löschen | `delete_area` kostet Zeit proportional zu den lebenden Zellen im Bereich (keine O(1)-Bereichslöschung; etwa 15 s für 40 Mio. Zellen); je vollständig gelöschtem u ein `DELETE`-Block; physisch entfernt erst `compact_file()` (Abschnitt 5.2) |
| Kontextmanager | `__exit__` committet auch nach einer Ausnahme im `with`-Block (Semantik der Referenz); ein Commit-Fehler wird mit der ursprünglichen Ausnahme verkettet, Handle und Sperre werden freigegeben |
| Plugins | Rückrufe in dieselbe DB während eines Schreibvorgangs nicht unterstützt (`RuntimeError`), auch nicht in `occupied_addresses` während `delete_record`; CUSTOM-Logik muss zur Laufzeit registriert werden |
| Nicht konsolidierte Sparse-Anker | das Lesen eines Sparse-`u`, dessen Zellen in vielen einzelnen Blöcken geschrieben wurden, baut einen v-Index dieses `u` im Speicher auf: ≈ 60 Byte je lebendem Einzelzellen-Record, ≈ 140 Byte je Mehrzellen-Record; Kompaktierung oder Batch-Schreiben bündeln die Zellen; die gecachten Indizes begrenzt `[cache] vindex_max_marks`, ein einzelner Index über dem Budget wird dennoch allein gehalten (Abschnitt 9.4) |
| Blockverzeichnis | abgelöste Verzeichnisblöcke bleiben bis zur Kompaktierung in der Datei; Verzeichnisblöcke zählen zur Größengrenze; nach einem gescheiterten Zusammenführen (beschädigte Seite) „degraded“ – nur noch Läufe der Stufe 0, bis `repair_file` das Verzeichnis verwirft (Abschnitt 7.10) |
| Große Blöcke | ein cachebarer Block wird beim ersten Zugriff je Objekt einmal ganz gelesen und geprüft; ein DATA-Block über dem größten cachebaren Eintrag (Standard 8 MiB) wird ab 0.28.2 in Stücken zu 1 MiB geprüft und danach nur bereichsweise gelesen (Speicher ≤ 1 MiB statt Blockgröße). Neue dichte Schreibaufrufe erzeugen keine Blöcke über `[write] max_block_bytes` (8 MiB); Scans, Indexaufbau und Sparse-Blöcke lesen einen Block weiterhin ganz |
| `readonly`-Leser | sehen den Stand beim Öffnen; kein `refresh()`, für einen neueren Stand neu öffnen |
| Indexaufbau | sammelt Schlüssel und Postings bis `[index] build_memory_bytes` (512 MiB) im Speicher und lagert darüber sortierte Läufe in eine anonyme temporäre Datei aus (Plattenbedarf bis zur Größe der Sammlungen). Das Budget wird alle 256 Datensätze bzw. 64 KiB und alle 256 Sparse-Gruppen geprüft; der gerade gelesene Block liegt ganz im Speicher. Nicht im Budget: Deduplikationsmengen (nur für sich überschneidende Blöcke), Sparse-Overlays indizierter BASE-Felder, Sparse-Zellen indizierter Extension-Felder und die kodierte Posting-Liste des gerade geschriebenen Schlüssels (≈ 4–8 Byte je Dokument; FULLTEXT-Dokumentlängen: ein Eintrag je Dokument); Teile großer Schlüssel aus Läufen werden eintragsweise gemischt (Abschnitt 12.2) |
| Speicher der Indizes beim Lesen | Seiten der Schlüsselbäume, Posting-Caches und FULLTEXT-Dokumentlängen (seitenweise) sind budgetiert (Abschnitt 9.8), aber nicht alles: Ergebnis- und Vereinigungsvektoren wachsen mit der Ergebnisgröße (`sql()`; abschnittsweise über `sql_iter`/Ergebnisdateien, Abschnitt 14.5); die Diagnose-API `_ensure_secondary_directory` liefert alle Schlüssel |
| Präfixprüfung | vom Verzeichnis abgedeckte Blöcke werden beim ersten Laden an ihren Eintrag gebunden (Kopf und Präfix); Datenblöcke im gescannten Bereich sind seit 0.28.2 durch die Präfixprüfsumme geschützt (beim Öffnen geprüft). Ein Block mit absichtlich neu berechneten Prüfsummen wird nicht erkannt (siehe nächste Zeile) |
| **Warnung: gefälschte Verzeichnisse** | Die Prüfsummen (CRC32, Präfixprüfsumme CRC-32C) erkennen zufällige Schäden, keine absichtlichen Änderungen. Wer die Datei mit eigenen Werkzeugen bearbeitet und die Prüfsummen neu berechnet, kann Daten unbemerkt verfälschen – in Datenblöcken ebenso wie im Blockverzeichnis. Insbesondere wird ein Block im Lesebetrieb **einmal** gegen seinen Verzeichniseintrag geprüft, nicht je Eintrag: ein zusätzlicher, gefälschter Eintrag, der auf einen schon geprüften Block zeigt, wird nicht erneut abgeglichen und kann Daten eines anderen Realms oder Bereichs liefern. Im normalen Betrieb (auch nach Abstürzen, Bitfehlern oder fehlgeleiteten Schreibvorgängen) entsteht ein solcher Zustand nicht; `verify(deep=True)` vergleicht das gesamte Verzeichnis mit den Datenblöcken und erkennt ihn. **Für Programmierer:** Datenbankdateien nie mit eigenen Werkzeugen ändern; Dateien unbekannter Herkunft vor der Nutzung mit `verify(deep=True)` prüfen; Schutz vor unbefugter Änderung nur über Dateisystemrechte bzw. Verschlüsselung des Datenträgers – NHyDB ist kein Sicherheitsprodukt, verschlüsselt ist nur der Layoutplan. |
| Offene Handles | ein Handle prüft jeden Block einmal und nutzt ihn danach ohne erneute CRC; eine Änderung der Datei an Ort und Stelle durch einen anderen Prozess (auch `repair_file(allow_data_loss=True)` mit anschließendem Schreiben) bemerkt ein bereits offenes Handle nicht – neu öffnen (Abschnitt 9.8) |
| Negative Koordinaten | Einzelzugriffe (`get`, `get_value`, `get_cell`) liefern für negative `realm`/`u`/`v` `None` wie v0.14; Schreiben und N-Berechnung lehnen ab, die NumPy-Pfade melden `ValueError` |
| Öffnungsoption `strict` | entfernt (hatte keine Wirkung); stattdessen `readonly=True`; das Öffnen ist immer fail closed |
| Formatversionen | gelesen wird nur das Format 0.28.2 (keine Rückwärtskompatibilität in der Entwicklungsphase); Dateien des Formats 0.16 (`NHYDB016`, NHyDB 0.28.0), ein unveröffentlichtes Entwicklungsformat (`NHYDB017`) und ältere Formatrevisionen ⇒ `CorruptionError` mit Nennung des Formats, die Datei bleibt unverändert; kein Konverter – aus den Quelldaten bzw. Exporten neu aufbauen |
| Datenbanken von 0.27.1 | Dateikennung `NHYDB016`: Öffnen ⇒ `CorruptionError` „unsupported file format NHYDB016 …“ bereits am Superblock (auch `readonly=True`); die Prüfung auf einen Schema-Digest ohne Schlüssel im Layoutplan (Hinweis auf 0.27.1) besteht weiter; keine Migration – mit 0.27.1 exportieren, mit 0.28.2 neu importieren (Abschnitt 7) |
| ID-Bereiche | `shape_id`, `layout_id` (auch MATH), `index_id`, `default_shape_id` ≤ 2^63−1 (`ValueError`); `block_id` ≤ 2^64−1; Enum-Codes `i64`; mathematische Koordinaten und `varuint`/`varint`-Werte unbeschränkt (Abschnitt 3.6) |
| NumPy-Koordinaten | `u`/`v`/`object_id`-Arrays auf `uint64` begrenzt |
| Dense-Batch | Recordbereich ≤ 4 GiB je Block |
| Plan-Generationen | nur zwei Generationen im Plan; Superblock-Rückfall über mehr als eine Schema-Generation ⇒ fail closed |
| Repair | opfert committete Daten nur mit `allow_data_loss=True`; unkommittierte Blöcke werden standardmäßig verworfen, mit `keep_uncommitted=True` übernommen (Abschnitt 15.4) |
| Durability `full` | macOS: `F_BARRIERFSYNC` vor und `F_FULLFSYNC` nach dem Superblock, bei fehlender `F_FULLFSYNC`-Unterstützung des Dateisystems ein Fehler (kein stiller Rückfall); Linux: `fsync(2)`; Windows: `FlushFileBuffers` (Abschnitt 9.6) |
| Plattformen | primär auf macOS (Apple Silicon) entwickelt; die CI baut und testet auf Linux, macOS und Windows |
| Indizes | nur CLASSIC-Records werden indiziert; Indizes werden standardmäßig nach Commits automatisch gepflegt (`[index] maintenance = "auto"`, Abschnitt 12.2); mit `maintenance = "manual"` baut sie nur `build_indexes()` neu. Ein Index auf ein Feld, das im aktuellen BASE-Shape fehlt, liest nur SPARSE-Zellen; Dense-Werte aus älteren Shapes stehen nicht im Index |
| RAM-Limiter unter Windows | bei einem Limiter-Budget, das nicht erreichbar ist, kann eine Cache-Entlastung das Cache-Budget mehrere Sekunden bis etwa zwei Minuten begrenzt halten (gemessen 4–131 s), bis die Entlastung als unwirksam erkannt ist; das Verhalten ist beabsichtigt (Abschnitt 9.9) |
| NHySQL | nur lesend, nur CLASSIC-Quellen; `DISTINCT`, `HAVING` nicht implementiert |
| Volltextsuche | `search` setzt einen frischen FULLTEXT-Index voraus (kein Scan-Fallback) |
| Große Sortierungen und Ergebnisse | Streaming-Wege sortieren jedes `ORDER BY` begrenzt (Speicher bis `sort_memory_bytes`, darüber Läufe auf der Platte im Rahmen von `budget_bytes`); NaN über dem Budget ⇒ `QueryLimitError`. `sql()`/`execute()`/`search()`/`sql_numpy()` sind durch `[query] max_result_bytes` (4 GiB) begrenzt. Der allgemeine Weg hält die Liste der Kandidaten-N (≈ 45 Byte je Zeile) im Speicher (Abschnitt 14.5) |
| Selektives Dekodieren | Feldabfragen prüfen nicht gewählte Felder eines Datensatzes nicht inhaltlich (etwa UTF-8); Bitfehler erkennt die Block-CRC, inhaltlich ungültige Werte in einem CRC-gültigen Block zeigt `verify(deep=True)` |
| Ergebnisdateien und Sperren | geteilte/exklusive Dateisperren (`flock`); auf Dateisystemen ohne Sperren fehlt der Schutz vor `cleanup_results` anderer Prozesse. Unter Windows sind Dateisperren verbindlich; die Besitzersperre liegt dort auf einem einzelnen Byte bei Offset 2^63−1, außerhalb des Dateiinhalts, sodass der Schreiber nicht durch seine eigene geteilte Sperre blockiert wird. Verdeckte Reste abgestürzter Prozesse räumt `cleanup_results` unter Windows nicht auf. |
| Gezielte Manipulation | ungeschlüsselte Prüfsummen: ein Block mit neu berechneten Prüfsummen und verschobenem u-Bereich liefert ohne Blockverzeichnis still eine ältere Version oder „nicht vorhanden“; `verify()` erkennt es (siehe Warnung oben) |

### 19.2 Fehlertexte

Fehler aus intern verwendeten Bibliotheken erscheinen mit der passenden
NHyDB-Fehlerklasse und eigenem Text, z. B.
`CorruptionError("layout plan: decryption/authentication failed (invalid AES-GCM tag)")` bei einem
nicht entschlüsselbaren Layoutplan oder
`MathSyntaxError("invalid syntax (<unknown>, line 1)")` bei Syntaxfehlern in
MATH-Ausdrücken. Alle Fehlertexte der Engine sind englisch. Fehlertexte sind
kein Teil der stabilen Schnittstelle und können sich ändern (Schreibweise,
Zusätze wie Pfade oder Hinweise); Programme sollen Fehlerklassen und – bei
`OSError` – `errno` auswerten, nicht den Text.

---

## 20. Anhang: Referenzen im Quellcode

| Thema | Datei |
|---|---|
| Adressmathematik | `crates/nhydb-core/src/address.rs`, `nat.rs` |
| Datentypen | `crates/nhydb-core/src/codec.rs`, `val.rs` |
| Containerformat | `crates/nhydb-core/src/format.rs`, `storage.rs`, `db/mod.rs` |
| Blockverzeichnis | `crates/nhydb-core/src/db/directory.rs`, `db/segindex.rs` |
| Caches | `crates/nhydb-core/src/db/blockcache.rs`, `db/vindex.rs` |
| R0,u1 und Layoutplan | `crates/nhydb-core/src/db/system.rs`, `crypto.rs` |
| Dense/Sparse | `crates/nhydb-core/src/db/dense.rs`, `db/sparse.rs`, `db/vindex.rs`, `db/interval.rs` |
| Layouts | `crates/nhydb-core/src/db/layout.rs`, `schema.rs`, `plugin.rs` |
| Lesen | `crates/nhydb-core/src/db/read.rs`, `db/materialize.rs`, `db/cursor.rs`, `db/columns.rs` |
| Indizes | `crates/nhydb-core/src/db/index.rs`, `db/secindex.rs` (Schlüsselbäume), `db/idxspill.rs` (externe Sortierung), `textindex.rs`, `setops.rs`, `unicode.rs` |
| MATH | `crates/nhydb-core/src/mathexpr.rs`, `mathlayout.rs`, `db/math.rs` |
| Abfragen | `crates/nhydb-core/src/query.rs`, `sql.rs` |
| Wartung | `crates/nhydb-core/src/db/maint.rs`, `db/merge.rs` (speicherbegrenzte Sparse-Zusammenführung) |
| Plattform, Konfiguration | `crates/nhydb-core/src/platform.rs`, `config.rs` |
| Python | `crates/nhydb-py/src/*.rs`, `python/nhydb/*.py` |
| Tests | `crates/nhydb-core/tests/*.rs`, `tests/python/test_*.py` |
| Beispielkonfiguration | `config/nhydb_config.example.toml` |
| Build | `BUILDING.md`, `pyproject.toml`, `Cargo.toml` |
