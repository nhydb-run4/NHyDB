# NHyDB 0.27.1 – Notebook-Reihe v1 (deutsche Fassung)

Eine interaktive Einführung in **NHyDB**, die Datenbank, in der jeder Wert eine mathematische
Adresse hat, und in die **Dynadron**-Mathematik dahinter. Jedes Notebook verbindet kurze
Erklärungen, ausführbaren Code gegen die echte NHyDB-0.27.1-API, Diagramme aus echten
Engine-Daten und kleine Experimente zum Selbst-Ändern.

> Erstellt für **NHyDB 0.27.1**. Mit anderen Versionen können die Ausgaben abweichen.
>
> 🇬🇧 **English:** Diese Fassung ist inhaltlich identisch mit der englischen Reihe in
> [`../en/`](../en/README_v1.md). Übersetzt sind die Begleittexte. **Code, Code-Kommentare, Programmausgaben und
> Diagrammbeschriftungen sind englisch** – genau wie in der Originalfassung.

## Inhalt

### Hauptreihe (`main/`)

| Nr. | Notebook | Leitfrage |
|---:|---|---|
| 00 | [Willkommen bei NHyDB](main/00_start_here_v1.ipynb) | Was ist das, und wo fange ich an? |
| 01 | [Die Adresse ist die Identität](main/01_the_address_is_the_identity_v1.ipynb) | Warum ist jede Zelle genau eine Zahl? |
| 02 | [Von der Mathematik zu gespeicherten Werten](main/02_from_address_to_data_v1.ipynb) | Wie speichere ich dort etwas? |
| 03 | [Datensätze, Felder und Lanes](main/03_records_fields_and_lanes_v1.ipynb) | Wie sieht das Datenmodell aus? |
| 04 | [Ein Datenbestand, verschiedene Layouts](main/04_layouts_side_by_side_v1.ipynb) | Wie werden Daten auf Zellen verteilt? |
| 05 | [MATH_MAP: ein Text, eine Formel, eine Datenbank](main/05_math_map_text_and_formula_lab_v1.ipynb) | Wie wähle ich den Ort über eine Formel? |
| 06 | [Wissenschaftliche Ergebnisse in Zellen (MATH_LONG)](main/06_scientific_results_in_cells_v1.ipynb) | Wie speichere ich Ergebnisse von Berechnungen? |
| 07 | [Riesige Adressen, exakte ganze Zahlen](main/07_realms_and_huge_numbers_v1.ipynb) | Wie groß ist der Adressraum wirklich? |
| 08 | [Adressen sind keine Suchindizes](main/08_addresses_are_not_indexes_v1.ipynb) | Wie finde ich Daten über ihren Inhalt? |
| 09 | [Fragen an die Daten](main/09_queries_and_sql_v1.ipynb) | Wie frage ich Daten ab? |
| 10 | [Von einzelnen Zellen zu NumPy-Batches](main/10_numpy_batches_v1.ipynb) | Wie gehe ich mit Massendaten um? |
| 11 | [Was liegt eigentlich auf der Platte?](main/11_what_lives_on_disk_v1.ipynb) | Warum sind die Daten sicher gespeichert? |
| 12 | [Ein Layoutplan, zwei Ansichten (ENC und KEY)](main/12_encrypted_layout_and_key_v1.ipynb) | Was schützt der geschützte Layoutplan, und wie? |
| 13 | [Messen statt vermuten (Benchmark-Labor)](main/13_benchmark_lab_v1.ipynb) | Wie schnell ist es auf meinem Rechner? |
| 14 | [Eine kleine echte Anwendung bauen](main/14_complete_application_v1.ipynb) | Wie sieht eine vollständige Anwendung aus? |

**Lesepfade**

* **Kernpfad** (etwa eine Stunde): 00 → 01 → 02 → 05 → 12
* **Mathematik-Pfad:** 01 → 05 → 06 → 07
* **Entwickler-Pfad:** 02 → 03 → 04 → 08 → 09 → 10 → 11 → 13 → 14

Jedes Notebook lässt sich auch für sich allein lesen. Einsteiger lesen von oben nach unten;
Erfahrene überspringen die mit 📘 *Hintergrund* markierten Abschnitte und gehen direkt zu den
mit ▶ markierten Experimenten; optionale Vertiefungen sind mit ⏩ *Vertiefung* markiert.

### Cookbook-Referenzlabor (`reference/`)

Basierend auf dem offiziellen englischen
[NHyDB Python Cookbook](https://github.com/nhydb/NHyDB/blob/main/docs/cookbook/PYTHON_COOKBOOK.md),
das die maßgebliche Referenz bleibt: alle 57 ausführbaren Cookbook-Beispiele, jedes als
eigenständige Code-Zelle mit der erwarteten Ausgabe darunter. Code und erwartete Ausgaben sind
unverändert, die Begleittexte übersetzt.

| Notebook | Cookbook-Abschnitte |
|---|---|
| [R01 Grundlagen](reference/R01_basics_v1.ipynb) | 1–8: Installation, erste Datenbank, Optionen, Commit, Datensätze, Zellen, Erweiterungen |
| [R02 Adressen, Typen, Bereiche](reference/R02_addresses_types_ranges_v1.ipynb) | 9–12: Adressen und N, Lesen über N, Datentypen, Bereiche und Iteratoren |
| [R03 Indizes, Abfragen, SQL](reference/R03_indexes_queries_sql_v1.ipynb) | 13–15: Sekundärindizes, QueryEngine, NHySQL |
| [R04 NumPy und Layouts](reference/R04_numpy_and_layouts_v1.ipynb) | 16–17: NumPy-Batch-API, COLUMN/PACKED/CUSTOM, Routing, Evolution |
| [R05 MATH_MAP](reference/R05_math_map_v1.ipynb) | 18: MATH_MAP |
| [R06 Betrieb](reference/R06_operations_v1.ipynb) | 19–24: Konfiguration, Wartung, Dateien und Backup, Fehler und Sperren, Systembereich, CLI |
| [R07 Grenzen und API-Referenz](reference/R07_limitations_and_api_reference_v1.ipynb) | 25–27: Anwendungsbeispiele, bekannte Einschränkungen, API-Referenz |

Die ausführliche deutsche Technische Dokumentation zu NHyDB liegt in diesem Repository unter
[`docs/de/NHyDB_Technische_Dokumentation.md`](../../docs/de/NHyDB_Technische_Dokumentation.md).

## Loslegen

**1. Notebooks holen.** Klone das Repository (oder lade es auf GitHub als ZIP-Datei herunter):

```bash
git clone https://github.com/nhydb/NHyDB.git
cd NHyDB
```

**2. Python-Pakete installieren.** Du brauchst Python 3.9 oder neuer. Eine virtuelle Umgebung
trennt die Installation von deinem System-Python (empfohlen):

```bash
python -m venv .venv
source .venv/bin/activate          # Windows: .venv\Scripts\activate
python -m pip install "nhydb==0.27.1" numpy pandas matplotlib jupyterlab ipykernel
```

**3. JupyterLab starten** im Notebook-Ordner:

```bash
cd notebooks/de
python -m jupyterlab
```

JupyterLab öffnet die Notebook-Oberfläche in deinem Webbrowser. Öffne im Dateibrowser links
`main/00_start_here_v1.ipynb` und arbeite die Notebooks nacheinander durch:

* **Lesen:** Die Notebooks sind mit ihren Ausgaben und Diagrammen gespeichert – du kannst sie
  lesen, bevor du irgendetwas ausführst.
* **Ausführen:** eine Zelle mit *Umschalt + Enter*, alle Zellen mit *Run → Run All Cells*.
* **Verändern:** Ändere Werte in einer Zelle und führe sie erneut aus – die meisten Notebooks
  enthalten Experimente und kleine Aufgaben genau dafür.

Zum Beenden den Browser-Tab schließen und im Terminal *Strg + C* drücken.

Alle Notebooks legen ihre Datenbanken in einem temporären Ordner an und löschen ihn am Ende;
keine Datei außerhalb dieses Ordners wird angefasst. Die Zeitmessungen in den Notebooks 08, 10
und 13 stammen von einem MacBook Air (Apple M2) und fallen auf anderen Rechnern anders aus.

**Automatisierte Ausführung (optional).** Um Notebooks ohne Browser-Oberfläche auszuführen, etwa
als Test, verwende `jupyter execute` im Ordner `notebooks/de/`:

```bash
jupyter execute main/*.ipynb reference/*.ipynb
```

## Lizenz

Die Notebooks verwenden NHyDB, lizenziert unter der *NHyDB / dNHyDB Experimental Software
License 1.0* (Quellcode einsehbar, nicht für den Produktiveinsatz). Website: https://nhydb.de/nhydb/
