# Zmiany


## [1.4.2] - 2026-11-04

### Nowości
- **Starsze wersje SQL Server przez TLS 1.0/1.1:** DBine łączy się z SQL Server 2012 i wcześniejszymi, gdy serwer obsługuje tylko TLS 1.0 lub 1.1. Zawsze najpierw próbuje TLS 1.2 lub nowszego. Jeśli serwer akceptuje tylko stary protokół, DBine informuje o tym (podając wersję serwera) i jednorazowo pyta, czy mimo to się połączyć; po potwierdzeniu pobiera komponent **SQL Server (TLS 1.0/1.1)** i zapamiętuje wybór dla tego połączenia. Gdy jest używany, widoczny jest wskaźnik **Starszy TLS**, certyfikat nadal jest weryfikowany (lub przypinany za pomocą **Ufaj certyfikatowi serwera**), a opcję można wyłączyć na karcie **SSL/TLS** połączenia. Aktualizacja serwera do TLS 1.2 (SQL Server 2012 SP4) nadal jest bezpieczniejszym rozwiązaniem.

### Ulepszenia
- **Logotypy silników:** kolejne 51 typów połączeń pokazuje logo swojego produktu zamiast inicjałów, m.in. IBM Db2, Informix i Netezza, Vertica, Exasol, InterSystems IRIS, Microsoft Access, Greenplum, StarRocks, OceanBase, TDengine, Materialize, Memgraph, RisingWave i Dremio.

## [1.4.1] - 2026-11-03

### Nowości
- **Automatyczne aktualizacje:** DBine sprawdza dostępność nowej wersji przy uruchomieniu i co 6 godzin, pobiera ją i weryfikuje w tle, a gdy jest gotowa, pokazuje na pasku stanu **Uruchom ponownie, aby zaktualizować**. Jeśli po prostu zamkniesz aplikację, aktualizacja zainstaluje się przy zamknięciu, a kolejne uruchomienie będzie już nową wersją, z podsumowaniem nowości. Instalacje, które nie potrafią same się aktualizować (deb, rpm, MSI, Flatpak), nadal pokazują powiadomienie z linkiem do pobrania. `DBINE_AUTO_UPDATE=0` wyłącza automatyczne aktualizacje na zarządzanych komputerach.
- **q w edytorze:** zapytania KDB+ mają podświetlanie składni q oraz uzupełnianie słów kluczowych, funkcji wbudowanych, tabel, kolumn, funkcji i nazw z przestrzenią nazw, takich jak `.u.upd`. ⌘/ komentuje wiersze za pomocą `/`, a fragmentów kodu q jest więcej.
- **Porównywanie, synchronizacja i konwersja schematów KDB+:** porównuj i synchronizuj tabele w pamięci (dodawanie, usuwanie, zmiana typu i zmiana nazwy kolumn, zmiana klucza), zmieniaj nazwy tabel i konwertuj schematy między kdb+ a innymi silnikami, z typami q i tabelami z kluczem odwzorowanymi na klucze główne. Tabele partycjonowane i splayed są odrzucane, z podaniem przyczyny.

### Ulepszenia
- **Klucze obce, ładowanie zbiorcze i wykresy KDB+:** powiązane kolumny są widoczne jako klucze obce na diagramie ER i przy przechodzeniu po kluczu (**Przejdź do wiersza, do którego prowadzi odwołanie**), importy i migracje ładują wiersze jako tabele z typami przez IPC (100 000 wierszy w około sekundę), a wykresy są agregowane na serwerze na podstawie całego wyniku.
- **Logo KX:** połączenia KDB+ pokazują logo KX.

## [1.4.0] - 2026-11-03

### Nowości
- **Obsługa KDB+:** połącz się z procesami kdb+ (bez szyfrowania lub przez TLS) i pracuj w q: przeglądaj tabele, widoki, funkcje i zmienne każdej przestrzeni nazw, uruchamiaj skrypty q instrukcja po instrukcji, filtruj, sortuj i edytuj wiersze tabel w siatce oraz śledź pamięć i otwarte połączenia w Monitorze. W kdb+ 3.6 i nowszych połączenia tylko do odczytu są dodatkowo wymuszane przez serwer (`reval`); na starszych serwerach połączenie tylko do odczytu otwiera wyłącznie tabele i zmienne. **Kopiuj parametry połączenia** podaje uchwyt q `hopen` i PyKX.
- **Logowanie do Databricks przez przeglądarkę:** połączenia Databricks i Azure Databricks mogą logować się przez **Logowanie w przeglądarce (SSO)**: DBine otwiera stronę logowania workspace'u (Microsoft Entra ID na Azure), więc SSO i MFA Twojej organizacji działają bez osobistego tokenu dostępu i bez service principal. Sesja odnawia się sama, dopóki DBine jest otwarty.

### Ulepszenia
- **Uprawnienia do nadania pochodzą z serwera:** w **Użytkownicy i uprawnienia…** lista uprawnień to te, które dany serwer może nadać; jest odczytywana z jego własnego katalogu, jeśli go ma (`sys.fn_builtin_permissions` w SQL Server, ACL w PostgreSQL, `SHOW PRIVILEGES` w MySQL, ClickHouse, Oracle, SAP HANA, Redis, MongoDB, Neo4j, Couchbase, OpenSearch…), a w pozostałych przypadkach z dokumentacji silnika, więc podąża za wersją serwera: SQL Server oferuje teraz `VIEW DATABASE STATE`, `VIEW DATABASE PERFORMANCE STATE`, `SHOWPLAN` i inne. Pokazuje tylko to, co dotyczy wybranego celu (cała baza danych, schemat, tabela, procedura…), a ta sama lista zasila **Nowy schemat…**. Uprawnienie nadal można wpisać ręcznie.

### Poprawki
- **Bezpieczniejszy import arkuszy kalkulacyjnych:** odczyt plików `.xls` i `.xlsb` korzysta z calamine 0.38, więc spreparowany plik nie może już zawiesić DBine ani wyczerpać jego pamięci (RUSTSEC-2026-0436).
- **Pola tekstowe zachowują dokładnie to, co wpisujesz:** nazwy użytkowników, hasła, nazwy hostów, adresy URL połączeń, wyszukiwania, filtry i wszystkie inne pola tekstowe nie dostają już od systemu operacyjnego wielkiej pierwszej litery, autokorekty ani sprawdzania pisowni, więc `demo_user` pozostaje `demo_user`. Shift i Caps Lock działają jak zawsze. To samo dotyczy filtra połączeń w aplikacji na telefon.

## [1.3.1] - 2026-11-02

### Ulepszenia
- **Odzyskiwanie połączenia po przerwie w sieci:** połączenie, które zakończyło się błędem lub zawisło podczas łączenia, można teraz rozłączyć z jego menu, co zamyka jego sesje i tunel SSH; ponowne połączenie zaczyna się od zera. Zapytanie, które kończy się błędem, bo połączenie zostało zerwane, odrzuca teraz swoją sesję, a gdy tunel SSH nie dociera już do serwera, także tunel, dzięki czemu następne zapytanie łączy się ponownie samo. Przed rozłączeniem program pyta, gdy karta ma otwartą transakcję lub wciąż wykonywaną instrukcję, a nawiązywanie połączenia i logowanie tunelu SSH nie mogą już czekać w nieskończoność.
- **Jaśniejszy komunikat, gdy nie można sprawdzić aktualizacji offline:** bez połączenia z internetem sprawdzanie lub pobieranie aktualizacji informuje teraz, że serwer aktualizacji jest nieosiągalny, zamiast pokazywać błąd biblioteki sieciowej.

### Poprawki
- **Tunele SSH, bezpieczeństwo:** biblioteka SSH została zaktualizowana do wersji usuwającej kilka luk opisanych w zaleceniach bezpieczeństwa; najpoważniejsza pozwalała kluczowi przechowywanemu w agencie SSH podpisywać bez prośby agenta o potwierdzenie. Serwer nie może już zawiesić tunelu ani sprawić, by DBine zużywał nieograniczoną ilość pamięci przy użyciu spreparowanych pakietów.

## [1.3.0] - 2026-10-30

### Nowości
- **Profile tunelu SSH:** zapisz tunel SSH (serwer, bastiony, użytkownika i uwierzytelnianie) raz jako profil w Ustawieniach i wybieraj go w dowolnym połączeniu; każde połączenie zachowuje własny serwer bazy danych. Zmiany w profilu obejmują wszystkie korzystające z niego połączenia przy następnym połączeniu, bieżący tunel połączenia można zapisać jako profil, a sekrety profilu pozostają w systemowym łańcuchu kluczy.
- **Kolumny tabeli po najechaniu kursorem:** po zatrzymaniu wskaźnika na nazwie tabeli w edytorze SQL wyświetlane są jej kolumny z typem, informacją o dopuszczaniu wartości null, kluczem głównym i komentarzem każdej kolumny, dla wszystkich silników.

### Ulepszenia
- **Asystent AI z Claude Code i Codex:** gdy pytasz o inną bazę danych tego samego połączenia, asystent odczytuje jej tabele i kolumny bez przełączania bazy danych karty. Te modele nadal nigdy nie otrzymują wierszy tabel.

### Poprawki
- **SAP HANA w systemie Linux, bezpieczeństwo:** anulowanie zapytania nie odczytuje już folderu zaufanych certyfikatów po tym, jak sesja go usunęła; w tej chwili inny użytkownik tego samego komputera mógł podłożyć własny urząd certyfikacji.
- **Bezpieczeństwo, zależności:** `ctutils` i `crypto-bigint` zostały zaktualizowane do wersji wykraczających poza ich poprawki dotyczące stałego czasu działania.
- **Eksport bardzo długiego tekstu do Excela:** komórka dłuższa niż limit Excela jest ponownie przycinana, aby się zmieściła, gdy zawiera znaki takie jak emoji, zamiast przerywać eksport.
- **Trino i Presto, bezpieczeństwo:** kopiowanie lub eksport danych z tabeli, której typ kolumny jest zagnieżdżony głębiej niż w jakimkolwiek rzeczywistym schemacie, nie może już zawiesić DBine; limit głębokości dodany w 1.2.2 obejmuje teraz także tę ścieżkę.

Podziękowania dla [@dylanwu](https://github.com/dylanwu), którego szczegółowe zgłoszenia doprowadziły do powstania trzech nowych funkcji w tej wersji.

## [1.2.2] - 2026-10-28

### Poprawki
- **Bezpieczeństwo: naprawiono krytyczną lukę w obsłudze ZIP.** 28 października opublikowano krytyczną lukę w `zip`, bibliotece, której DBine używa do odczytu i zapisu plików opartych na ZIP: importu i eksportu Excela oraz pobierania wbudowanego modelu asystenta AI ([GHSA-m665-v7pm-7grj](https://github.com/advisories/GHSA-m665-v7pm-7grj)). Specjalnie spreparowane archiwum mogło pokazywać różną zawartość w zależności od sposobu odczytu. Ta wersja przenosi wszystkie części DBine na naprawioną bibliotekę; ukazuje się kilka godzin po 1.2.1 wyłącznie z tego powodu, dlatego zalecamy aktualizację.
- **Trino i Presto, bezpieczeństwo:** typ kolumny zagnieżdżony głębiej niż w jakimkolwiek prawdziwym schemacie (jak mógłby wysłać złośliwy serwer) jest wyświetlany w takiej postaci, w jakiej dotarł, zamiast być analizowany poziom po poziomie, więc nie może już zawiesić DBine.

## [1.2.1] - 2026-10-28

### Poprawki
- **Uzupełnianie kolumn podąża za zapytaniem, które piszesz:** edytor podpowiada tylko kolumny tabel, CTE i podzapytań, które czyta instrukcja pod kursorem, a nie kolumny innych instrukcji w pliku. Kolumna wspólna dla dwóch takich tabel jest podpowiadana raz dla każdej tabeli, opisana aliasem i wstawiana z kwalifikatorem (`c.customer_id`); kolumny CTE i tabeli pochodnej są podpowiadane w zapytaniu oraz po jego aliasie.
- **Nazwy pól w wierszach i strukturach:** wartości `ROW` w Trino i Presto, typy złożone PostgreSQL oraz struktury Arrow Flight SQL pokazują każde pole pod jego nazwą, jako JSON, także w zagnieżdżonych wierszach, tablicach wierszy i mapach, których wartościami są wiersze (`[{"customer_id":42,"items":[{"product_name":"demo-product","quantity":2}],"note":null}]` zamiast `[[42,[["demo-product",2]],null]]`). `null`, pusty ciąg i pusta tablica nadal się różnią. Nazwane krotki ClickHouse zachowują zadeklarowaną kolejność pól.
- **Migracje z loginem SQL Server bez VIEW DEFINITION:** gdy login źródłowy mógł czytać tabele, ale nie ich definicje (częste w Azure SQL), skrypt migracji zapisywał `CHECK ()`, co kończyło się błędem "Incorrect syntax near ')'", i po cichu pomijał wartości domyślne kolumn. Teraz takie CHECK, wartości domyślne i wyrażenia obliczane są pomijane w skrypcie, a podgląd i raport z uruchomienia wymieniają każde z nich na początku, wraz z powodem i `GRANT VIEW DEFINITION`, który pozwala DBine je odczytać. Wygenerowane skrypty i synchronizacja schematów również o tym informują, a synchronizacja nie usuwa już CHECK w celu tylko dlatego, że źródło nie mogło odczytać własnego. Żaden silnik nie zapisuje już pustego CHECK ani DEFAULT.

## [1.2.0] - 2026-10-27

### Nowości
- **Ukraiński i polski:** DBine i aplikacja na telefon są dostępne w językach Українська i Polski, razem z komunikatami serwera i asystentem AI. Język wybiera się w **Ustawienia › Ogólne › Język**; w przeciwnym razie DBine używa języka systemu.
- **Indonezyjski:** DBine i aplikacja na telefon są dostępne w języku Bahasa Indonesia, razem z komunikatami serwera i asystentem AI. Język wybiera się w **Ustawienia › Ogólne › Język**; w przeciwnym razie DBine używa języka systemu.

### Ulepszenia
- **Uzupełnianie między bazami danych:** w silnikach, w których jedno zapytanie może czytać inną bazę tego samego połączenia (ClickHouse, MySQL i MariaDB, SQL Server, Snowflake, Databricks, Trino, BigQuery, Oracle i inne), edytor podpowiada nazwy innych baz, ich tabele po `other_db.` oraz ich kolumny po tabeli lub aliasie, także w złączeniach między bazami. Tabele każdej bazy są odczytywane przy pierwszym wpisaniu jej nazwy z kropką.
- **Silnik DuckDB zaktualizowany do 2.0.0:** lokalne pliki DuckDB i DataFrame SQL w notebookach działają teraz na DuckDB 2.0.0 (wcześniej 1.5.5). Nowe pliki baz używają formatu przechowywania DuckDB 2.0, którego starsze wersje DuckDB nie potrafią otworzyć; istniejące pliki nadal się otwierają i pozostają czytelne dla wersji, która je utworzyła.
- **Szyfrowanie połączenia dla SQL Server, PostgreSQL i MySQL:** formularz połączenia pokazuje opcje TLS właściwe dla każdego silnika, pod nazwami używanymi przez jego narzędzia: `Encrypt` (Optional, Mandatory, Strict), `TrustServerCertificate`, `ServerCertificate` i `HostNameInCertificate` dla SQL Server i Azure SQL; `sslmode` i `sslrootcert` dla PostgreSQL i jego rodziny; `ssl-mode` i `ssl-ca` dla MySQL i jego rodziny; `ssl`, `ssl-verify-server-cert` i `ssl-ca` dla MariaDB i OceanBase. Nowe połączenia są szyfrowane i sprawdzają certyfikat serwera, a każde ustawienie można wyłączyć. Zapisane połączenia działają dokładnie tak jak wcześniej. Przez tunel SSH certyfikat jest sprawdzany względem nazwy rzeczywistego serwera, a błąd certyfikatu wskazuje, którą opcję zmienić. `PREFERRED` w MySQL przechodzi na połączenie bez szyfrowania tylko wtedy, gdy serwer nie oferuje TLS, przed wysłaniem jakiegokolwiek hasła.
- **Szyfrowanie połączenia dla InfluxDB, IoTDB, ksqlDB, TDengine, Phoenix, Arrow Flight SQL i usług w chmurze:** formularz połączenia pokazuje opcje TLS właściwe dla każdego silnika: `ssl` i `unsafeSsl` dla InfluxDB 1, schemat adresu URL z `skip-verify` (InfluxDB 2) albo `tls-no-verify` i `tls-ca` (InfluxDB 3), `use_ssl` i `ca_certs` dla IoTDB i TimechoDB, `useTls`, `verifyHost` i `trustStore` dla ksqlDB, `useSSL` dla TDengine, schemat adresu URL, `hostname_verification` i `truststore` dla Phoenix i Avatica, `useEncryption` i `tlsRootCerts` dla Flight SQL oraz `DisableServerCertificateValidation` dla emulatora Cosmos DB; tam, gdzie narzędzia silnika nie mają nazwy dla pominięcia weryfikacji lub dla pliku CA, używane są `insecure` i `cacert` z curl. Nowe połączenia są szyfrowane i sprawdzają certyfikat serwera, a każde ustawienie można wyłączyć. Zapisane połączenia działają dokładnie tak jak wcześniej. Przez tunel SSH certyfikat jest sprawdzany względem nazwy rzeczywistego serwera, a błąd certyfikatu lub serwer bez TLS są zgłaszane jako takie. BigQuery, Spanner, DynamoDB, Athena, Snowflake, Databricks i libSQL zawsze używają weryfikowanego HTTPS, z wyjątkiem adresu emulatora wpisanego jako `http://`.
- **Klawiatura i czytniki ekranu:** widoczna ramka fokusu pokazuje, gdzie znajduje się klawiatura; wiersze list, karty, sortowalne nagłówki i zwijane sekcje są dostępne klawiszem Tab i obsługiwane klawiszem Enter lub spacją; menu kontekstowe działają ze strzałkami; przyciski z samą ikoną mają nazwy odczytywane przez czytnik ekranu; wynik zapytania (zakończone lub zakończone błędem) jest ogłaszany; animacje zatrzymują się, gdy system prosi o ograniczenie ruchu; a tekst pomocniczy ma wyższy kontrast w obu motywach.

### Poprawki
- **„Nie weryfikuj” teraz przypina certyfikat serwera, bezpieczeństwo:** połączenie, które nie weryfikuje certyfikatu serwera (`sslmode=require`, `insecure`, `bolt+ssc`, `validate=false`…), akceptuje certyfikat przedstawiony przez serwer za pierwszym razem, a potem już tylko ten, w PostgreSQL i jego rodzinie, silnikach HTTP (ClickHouse, Elasticsearch i OpenSearch, CouchDB, Trino, InfluxDB i inne), Neo4j, Neptune, Redis i Cassandra. Gdy certyfikat się zmieni, połączenie zostaje przerwane i pokazuje oba odciski, z przyciskiem **Zaufaj nowemu certyfikatowi** dla serwera, który go odnowił; karta SSL/TLS pokazuje przypięty odcisk z przyciskiem **Zapomnij**. Zapisane połączenia przypinają certyfikat przy następnym połączeniu. `verify-ca` i opcje pomijające tylko nazwę hosta sprawdzają teraz cały łańcuch względem CA zamiast akceptować dowolny certyfikat. SQL Server, MySQL, MariaDB, MongoDB i SAP HANA zachowują własne działanie swoich klientów.
- **Nowe bazy pojawiają się bez ponownego łączenia:** baza utworzona poleceniem `CREATE DATABASE` w zapytaniu (albo usunięta lub zmieniona) od razu pojawia się w eksploratorze i na listach baz, a otwarcie listy baz (migracja, porównanie, subset, kopie zapasowe, uprawnienia, wybór bazy w zapytaniu…) odczytuje ją ponownie, więc baza utworzona innym narzędziem też się pojawia.
- **Import z SQL Server Management Studio:** oprócz zarejestrowanych serwerów DBine importuje teraz serwery zapamiętane przez okno Connect w SSMS (jego historię połączeń), z loginem, bazą i ustawieniami szyfrowania, z SSMS 18–20 oraz z nowych folderów SSMS 21 i nowszych. W macOS i Linuksie okno wyboru plików przyjmuje teraz pliki `RegSrvr.xml`, `UserSettings.xml` i `.regsrvr` (oraz `dataSources.xml` z DataGrip); wcześniej można było wybrać tylko pliki JSON.
- **Import ciągów połączenia zachowuje ustawienia szyfrowania:** wklejony lub zaimportowany ciąg przenosi teraz swoje opcje TLS w postaci właściwej dla silnika: `Encrypt` w SQL Server (także `Strict`) i `HostNameInCertificate`, `sslmode` w PostgreSQL tak jak zapisano (`require` nie zmienia się już w połączenie weryfikowane), `ssl-mode` w MySQL i `useSSL` z Connector/J, `sslMode` w MariaDB oraz schematy `rediss://`, `bolt+s://` i `bolt+ssc://`. Ciąg bez ustawień TLS jest importowany tak, jak połączyłby się jego własny klient. To samo dotyczy połączeń importowanych z DBeaver, DataGrip i innych IDE JetBrains, DbGate oraz Azure Data Studio.
- **Błędy certyfikatu w macOS i Windows:** certyfikat serwera, któremu system nie ufa („The certificate was not trusted”, „issued by an authority that is not trusted”, „The target principal name is incorrect”), jest teraz rozpoznawany jako taki, a komunikat wskazuje, którą opcję TLS zmienić, zamiast ogólnego błędu TLS.
- **ClickHouse 22.8 i starsze:** łączenie nie kończy się już błędem `Unknown setting http_write_exception_in_output_format`; DBine przestaje żądać tego ustawienia od serwerów, które go nie mają.
- **Notebooki, eksport do Pythona:** połączenie PostgreSQL, które sprawdza certyfikat serwera, jest eksportowane z `sslmode=verify-full`, tak jak robi to **Kopiuj ciąg połączenia**, zamiast z `require`, które go nie sprawdza.
- **HashiCorp Vault, bezpieczeństwo:** adres Vault źródła haseł musi być `https://`; zwykły `http://` jest akceptowany tylko dla Vault na tym komputerze, tak jak w przypadku innych menedżerów sekretów, więc token Vault nigdy nie jest przesyłany siecią jawnym tekstem.
