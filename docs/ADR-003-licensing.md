# ADR-003 — Licencjonowanie V.I.C.T.O.R.

**Status:** przyjęta  
**Data:** 7 listopada 2026 r.

## Kontekst

V.I.C.T.O.R. ma być publicznie dostępny dla społeczności i użycia
niekomercyjnego, ale nie ma być darmowym zamiennikiem komercyjnego produktu
produkcyjnego. Potrzebujemy modelu source-available, który pozwala czytać,
rozwijać i używać kodu w społeczności, a komercyjne użycie produkcyjne kieruje
do osobnej, pisemnej licencji.

## Decyzja

Cały workspace V.I.C.T.O.R., z wyjątkiem wyraźnie oznaczonych komponentów
zewnętrznych, jest licencjonowany na BUSL-1.1. Parametry licencji są zapisane w
rootowym `LICENSE`:

- Licensor/copyright holder: Jarosław Kuchta;
- Licensed Work: V.I.C.T.O.R.;
- Additional Use Grant: użycie nieprodukcyjne oraz niekomercyjne/społecznościowe;
  komercyjne użycie produkcyjne nie jest objęte grantem i wymaga osobnej
  pisemnej licencji komercyjnej;
- Change Date: 7 listopada 2030 r.;
- Change License: Apache-2.0.

Nazywamy ten model **source-available**, a nie open source. Tekst BUSL-1.1 w
`LICENSE` pozostaje pełnym tekstem licencji; zmienione są wyłącznie jej jawne
parametry dotyczące tego utworu.

`vendor/bse-fetch` pozostaje osobnym komponentem MIT. Jego `LICENSE` i
`ORIGIN.md` są zachowane, a licencja workspace nie nadpisuje jego warunków.

## Konsekwencje

Metadane Cargo wskazują `BUSL-1.1`, README opisuje ograniczenia wprost, a każdy
przyszły komponent objęty inną licencją musi mieć własne, jednoznaczne oznaczenie
i zachowaną informację o pochodzeniu.
