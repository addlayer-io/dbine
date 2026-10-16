# Modifiche

## [0.1.13] - 2026-10-16

### Miglioramenti
- **Sostieni il progetto:** il pulsante a forma di cuore (**Sostieni il progetto**), il suo promemoria e **Aiuto › Sostieni il progetto** aprono Buy Me a Coffee.
- **Script con i dati di tabelle scelte:** in **Genera script del database**, con **Dati (INSERT)** selezionato, puoi mantenere i dati di tutte le tabelle o scegliere quali li ricevono, da un elenco con filtro (**Scegli tabelle…**), così lo script può contenere la struttura dell'intero database e le righe di sole alcune tabelle.
- **Menu del database:** **Esporta database…** è stato rimosso. Apriva la stessa finestra di **Genera script…**, che resta e fa tutto quello che faceva (struttura, dati e salvataggio su file).
- **Statistiche di utilizzo:** quando il server delle statistiche non risponde o fallisce, DBine riprova per un massimo di un'ora invece di scartare l'evento. Nient'altro di ciò che viene inviato cambia, e disattivare le statistiche interrompe qualsiasi nuovo tentativo in sospeso.
- **Download Flatpak:** ogni versione ora include un file `.flatpak` per Linux (`x86_64` e `aarch64`), da installare con `flatpak install --user ./dbine-X.Y.Z-x86_64.flatpak`; Flatpak scarica da solo il runtime GNOME da Flathub. Non si aggiorna da solo: installa il file della versione successiva allo stesso modo.

## [0.1.12] - 2026-10-15

### Novità
- **Flathub:** DBine è stato inviato a Flathub come `io.addlayer.dbine`. Quella build include tutti i driver, DuckDB e il motore dell'assistente IA, quindi al primo utilizzo non si scarica nulla, e a mantenerla aggiornata è Flatpak, non DBine. All'interno della sandbox, l'azione del firewall di Azure spiega che non può usare Azure CLI e offre il portale, T-SQL o un terminale, e le attività pianificate vengono salvate ma non possono essere registrate nell'utilità di pianificazione del sistema.
- **winget:** DBine è stato inviato a winget su Windows come `AddLayer.DBine` (`winget install AddLayer.DBine` una volta accettato), e ogni nuova versione ci arriva da sola. Installato con winget, DBine continua ad aggiornarsi da solo come prima.

### Miglioramenti
- **Nuovo identificatore dell'app:** DBine ora è `io.addlayer.dbine`. Al primo avvio dopo l'aggiornamento, connessioni, password salvate, schede aperte, chat dell'assistente, impostazioni e attività pianificate vengono copiate automaticamente; i dati della versione precedente restano com'erano, quindi tornarci non fa perdere nulla. Su Windows e Linux un grande modello di IA potrebbe essere scaricato di nuovo. Su macOS il portachiavi potrebbe chiedere l'accesso una volta, e alcuni permessi di privacy (come l'accesso alla rete locale) potrebbero essere richiesti di nuovo; su Windows, un blocco sulla barra delle applicazioni fatto con una versione precedente potrebbe dover essere rifatto.

## [0.1.11] - 2026-10-13

### Novità
- **Ordina per colonna:** fai clic sull'intestazione di una colonna nei dati di una tabella per ordinare in base ad essa (di nuovo per l'ordine decrescente, una terza volta per rimuoverlo; Shift+clic aggiunge altre colonne). Il database ordina l'intera tabella dove il motore lo consente, come per i filtri; altrove, o quando il server rifiuta l'ordinamento, vengono ordinate le righe caricate e un avviso ne spiega il motivo. I risultati di una query si possono ordinare allo stesso modo.
- **Controlli nella barra di stato:** in una scheda di query, la barra di stato blu ora contiene la connessione, il database, la modalità di transazione (Auto per impostazione predefinita, oppure Manuale) e il limite di righe. Fai clic su uno qualsiasi per cambiarlo con un selettore rapido, come in VS Code. La barra degli strumenti della query non perde più pulsanti quando la barra laterale è larga: ciò che non entra va sotto un menu **…**.
- **Invia feedback:** un pulsante in fondo alla barra delle attività apre un breve modulo per segnalare un bug, suggerire un miglioramento, richiedere un nuovo motore o fare una domanda. GitHub si apre con l'issue già compilata e con le etichette, quindi basta premere «Create». La diagnostica facoltativa (versione di DBine, sistema operativo, lingua, e motore e versione del server della connessione attiva) viene mostrata prima di essere inclusa, e non contiene mai server, utenti, nomi di database né query.

### Miglioramenti
- **Testo lungo nella griglia:** fermandosi su una cella il cui testo non entra, lo si vede per intero in un tooltip che mantiene le interruzioni di riga e che si può scorrere e selezionare.
- **Quando una connessione fallisce:** espandendo una connessione, prima si connette e mostra **Connessione…**; l'albero compare solo quando il server risponde (allora subito, da ciò che DBine ricorda, mentre si aggiorna). Se non riesce a connettersi, la connessione mostra un avviso con **Riprova** e **Vedi cosa succede…**, e le schede (i dati di una tabella, l'esecuzione di una query e gli altri strumenti) mostrano lo stesso errore invece di restare vuote. Se la connessione cade più tardi, torna all'avviso. **Vedi cosa succede…** spiega il guasto (firewall, timeout, rifiuto, nome host, certificato o login) e cosa controllare, e su Azure SQL può aggiungere il tuo IP al firewall del server con il tuo accesso Azure CLI, dopo aver confermato server, regola e IP. Funziona su tutti i motori; la causa è più precisa su SQL Server, Azure SQL, PostgreSQL e MySQL.

### Correzioni
- **Editor delle query:** una scheda la cui connessione non è ancora aperta (per esempio, ripristinata all'avvio) torna a completare i nomi di schemi, tabelle e colonne: chiedendo il completamento si apre la connessione e l'elenco compare quando arrivano i nomi. Gli snippet non entrano più nell'elenco dopo un punto.
- **Editor delle query:** l'elenco dei completamenti e i suggerimenti si aprono verso l'alto quando scrivi nelle ultime righe, invece di finire nascosti sotto i risultati.
- **Firewall di Azure SQL:** **Aggiungi il mio IP al firewall di Azure** usa solo l'IP pubblico di questa macchina, ottenuto via HTTPS e ricontrollato prima di creare la regola. Un IP indicato nel messaggio di errore di un server viene mostrato come non verificato e non viene mai usato.
- **Riprova dopo una connessione fallita:** se nel frattempo la scheda è passata a un'altra connessione o a un altro database, riprovando viene richiesta di nuovo la conferma di produzione.
- **Ridenominazione su Snowflake e su Trino, Presto e Starburst:** ciò che nomina l'oggetto rinominato (viste, viste materializzate, funzioni, procedure e, su Snowflake, task, stream e tabelle dinamiche) viene elencato perché lo correggi a mano invece di essere ricreato, così mantiene il suo proprietario e le impostazioni di sicurezza. La ridenominazione viene inoltre rifiutata quando il codice memorizzato si dividerebbe in più istruzioni, e la riscrittura dei dipendenti non cambia più l'identità con cui viene eseguito il codice (`DEFINER`, `SQL SECURITY`, `AUTHID`, `EXECUTE AS`).
- **Query dei client IA e dell'assistente:** una query viene eseguita senza chiedere approvazione solo quando il database la impone in sola lettura e chiama soltanto funzioni native del motore, senza effetti collaterali. Una che chiami qualsiasi altra funzione (una funzione utente, o una che agisce fuori dalla transazione, come sospendere la replica o prendere un lock) attende prima la tua approvazione.

## [0.1.10] - 2026-10-10

### Novità
- **I driver si aggiornano da soli, separatamente dall'app:** DBine cerca in un indice firmato il driver più recente compatibile con la tua versione, lo scarica in background e torna al precedente se qualcosa va storto. In Impostazioni › Driver c'è un pulsante **Cerca aggiornamenti**, lo stato di ogni driver e **Torna alla precedente**. Un driver può essere pubblicato da solo, senza una nuova versione dell'app.
- **Rinominare un database:** **Rinomina…** su un database nell'esplora risorse. La finestra mostra le altre sessioni aperte su di esso (che la ridenominazione termina), se il nuovo nome esiste già, quanti oggetti vengono spostati e lo script completo. Dopo, il database predefinito della connessione, le schede aperte, le query salvate, le migrazioni, le destinazioni dei progetti e i passaggi delle attività pianificate seguono il nuovo nome; le attività che modificano dati al suo interno chiedono di nuovo l'approvazione. Disponibile in SQL Server, Azure SQL, Babelfish, nella famiglia PostgreSQL, in MySQL, MariaDB, Snowflake e MongoDB; dove il motore non può rinominare (o spostare) un database, non viene offerto.
- **A quale colonna corrisponde questo valore?** In un `INSERT … VALUES`, posizionando il cursore su un valore compare un tooltip con la sua colonna (ad esempio "Colonna 14 di 48: Name") e quella colonna viene evidenziata nell'elenco. Senza elenco di colonne, usa quelle della tabella in ordine. Funziona su tutti i motori SQL e CQL.
- **Più modi per accedere a SQL Server con Microsoft Entra ID:** interattiva con MFA (nel browser), integrata (l'account Windows, tramite l'ADFS federato dell'organizzazione), identità gestita e predefinita (variabili d'ambiente, identità gestita, Azure CLI o Azure Developer CLI). Il token viene conservato finché DBine è aperto e rinnovato prima della scadenza.
- **Associare un login a un database:** **Associa login…** in Utenti e permessi crea l'utente del database per un login del server che esiste già (`CREATE USER … FOR LOGIN`, con il suo schema predefinito), elencando i login che lì non hanno ancora un utente. Disponibile su SQL Server, Azure SQL Database (lì il login si digita), Babelfish e SAP ASE.

### Miglioramenti
- **Cosa porta ogni versione:** l'avviso di nuova versione mostra le sue modifiche e quelle delle versioni intermedie, a partire da quella che hai installato, nella lingua dell'app.
- **Versioni vecchie dell'app:** d'ora in poi, un'app più vecchia delle ultime cinque versioni deve aggiornarsi per scaricare nuovi driver. I driver che ha già installati continuano a funzionare.
- **Utenti e permessi:** le modifiche vengono messe in coda e applicate insieme. Una barra **Modifiche in sospeso** le elenca, un'unica revisione mostra tutto lo script e vengono eseguite una alla volta: quelle applicate escono dall'elenco e, se una fallisce, lei e le successive restano in sospeso.
- **Le letture dei client MCP e dell'assistente IA sono protette dal database stesso:**
  - **Garantito dal server:** sulla famiglia PostgreSQL, MySQL, MariaDB, SQLite e libSQL, ogni lettura viene eseguita come una singola istruzione dentro una transazione in sola lettura che il server fa rispettare e che DBine annulla sempre.
  - **Approvazione:** sugli altri motori, ogni lettura chiede la tua approvazione in DBine, come le scritture. La finestra di approvazione indica se si tratta di una lettura o di una scrittura, e **Approva tutto** si applica separatamente a letture e scritture.
  - **Controllo delle scritture:** DBine continua a controllare ogni query alla ricerca di scritture, come livello aggiuntivo.

### Correzioni
- **La sola lettura è più rigorosa:** le query dei client MCP con livello Lettura, dell'assistente IA e delle connessioni in sola lettura vengono controllate parola per parola, non solo dalla prima. Ora rifiutano:
  - una scrittura nascosta dopo una lettura in un batch di SQL Server;
  - un `WITH` che modifica i dati;
  - `SELECT … INTO`, `EXEC` e `SET`;
  - funzioni che agiscono al di fuori della query, come `set_config`, `dblink_exec` e `xp_cmdshell`.

  Un piano stimato rifiuta gli script che disattiverebbero la modalità piano, quindi le alternative IA di **Ottimizza query** non possono eseguire nulla. Le attività pianificate i cui script ora contano come scrittura chiedono di nuovo l'approvazione.
- **Libreria con git:** un repository condiviso non può più far leggere, scrivere o eliminare a DBine file al di fuori della cartella della Libreria.
- **Tunnel SSH:** la chiave host di ogni server viene verificata separatamente. Una chiave accettata per un jump host non vale più per il server successivo, `known_hosts` viene controllato per primo e una chiave cambiata viene sempre rifiutata. Ai server che avevi accettato prima viene chiesto ancora una volta. La porta locale del tunnel serve solo i programmi del tuo stesso utente.
- **Backup nel cloud:** un file di backup modificato da qualcun altro non può più indebolire la cifratura del tuo prossimo caricamento.
- **libSQL / Turso:** un `authToken` in un URL incollato viene conservato nel portachiavi di sistema, non nell'indirizzo, nel nome o nella cronologia della connessione. Le connessioni salvate in precedenza vengono ripulite all'avvio di DBine.
- **Documenta il database:** i nomi delle colonne non possono iniettare HTML nel dizionario dati in Markdown.
- **Notifiche delle attività su Windows:** un messaggio di errore del database non può più eseguire comandi tramite la notifica. Anche le notifiche su macOS e Linux ricevono il testo come argomenti separati.
- **Tunnel SSH:** un server di cui hai accettato la chiave in DBine e che ora ne presenta una diversa viene rifiutato, invece di chiedertelo di nuovo. La sezione SSH della connessione elenca i server accettati, ciascuno con **Dimentica**.
- **SQL su PostgreSQL:** i valori di tipo stringa vengono scritti come `E'…'` con i backslash escapati, così un valore non può chiudere la stringa in anticipo su un server con `standard_conforming_strings` disattivato. Questo copre la famiglia PostgreSQL, CockroachDB e motori simili.
- **Script Snowflake:** un backslash all'interno di un `"nome tra virgolette"` non cambia più il punto in cui termina un'istruzione.
- **Esportazione CSV e TSV:** le celle di testo e i nomi di colonna che iniziano con `=`, `+`, `-`, `@`, una tabulazione o un ritorno a capo ricevono un `'` davanti, così i fogli di calcolo non li eseguono come formule. I numeri non vengono mai modificati. Un'opzione nella finestra di esportazione la disattiva.
- **Annullare le modifiche in Progetti:** un file con nome simile a un pattern (`*`) annulla solo quel file.
- **Copia un sottoinsieme:** il mascheramento usa una nuova chiave casuale a 256 bit a ogni esecuzione.
- **Aggiornamenti dei driver:** l'app non accetta mai un indice dei driver più vecchio di quello con cui è stata rilasciata, nemmeno su una nuova installazione, né uno che ha smesso di essere rinnovato. I driver installati continuano a funzionare in entrambi i casi.
- L'importazione delle connessioni, il linter e il controllo di integrità non si fermano più con caratteri accentati o altri caratteri multibyte.
- **Sola lettura su SQL Server:** ogni query viene eseguita in una transazione che viene sempre annullata. Backup, ripristini, attivazione o disattivazione dei trigger, scritture con puntatori di testo, Service Broker e istruzioni di transazione vengono rifiutati quando arrivano dopo una lettura nello stesso batch.
- **Sola lettura su PostgreSQL:** i nomi scritti con escape Unicode (`U&"…"`) vengono rifiutati, così una funzione vietata non può essere chiamata con un'altra grafia.
- **Esportazione CSV e TSV:** la protezione dalle formule vale anche per il testo salvato in colonne dichiarate numeriche, cosa che SQLite consente.
- **Script generati:** i nomi di oggetti provenienti dal server non possono chiudere un commento ed essere eseguiti come codice. Questo copre le correzioni suggerite dal **Controllo di integrità** e gli script di utenti, backup e struttura. I nomi ClickHouse con un backslash vengono racchiusi correttamente tra virgolette.
- **Sola lettura:** un'istruzione che non inizia con una parola, come un nome tra parentesi quadre che esegue una procedura su SQL Server, viene rifiutata. Vengono controllate anche le parole all'interno delle letture tra parentesi.
- **Modifica tabella:** gli avvisi dello script che apri come query restano sulla propria riga di commento.
- **Le password e le opzioni segrete** vengono mascherate ovunque si modifichino, compresi i passaggi di backup delle attività pianificate.
- **Mostra dipendenze e Rinomina** non si fermano più sulle routine con nomi tra virgolette insoliti.
- **Sola lettura su SQL Server:** un batch può iniziare solo con `SELECT`, `WITH`, `USE` o `PRINT`. Ciò che segue `SHOW`, `DESCRIBE` o `PRAGMA` viene controllato su tutti i motori.
- **Importa connessioni:** un URL JDBC di Oracle con utente e password li tiene fuori dal nome della connessione: la password va nel portachiavi di sistema.
- **Sola lettura su SQL Server:** una procedura il cui nome inizia come `print_` o `select1` non viene più scambiata per una lettura.
- **Importa connessioni:** una password Oracle contenente `@` viene conservata per intero nel portachiavi del sistema.
- **Tunnel SSH su Linux:** la porta locale del tunnel si fida solo delle connessioni aperte dal tuo stesso utente.
- **Sola lettura:** una query con un ritorno a capo isolato o uno spazio Unicode insolito viene rifiutata, perché i motori non concordano su dove lì termini un commento o un'istruzione. Vengono rifiutate anche le funzioni con effetti collaterali: `load_extension` in SQLite, `pg_notify` e le letture di replica logica in PostgreSQL, e l'annullamento di sessioni o query in Snowflake.
- **Importa connessioni:** un URL di SQL Server senza host non mette più la sua password nel nome della connessione.
- **Utenti e permessi:** l'anteprima nasconde le password di PostgreSQL in tutte le loro forme.
- **Sola lettura:** le query che acquisiscono lock su cui altre sessioni restano in attesa vengono rifiutate, per esempio `pg_advisory_lock`, `GET_LOCK`, `LOCK IN SHARE MODE` e `FOR SHARE`.
- **Snowflake e Cassandra:** i commenti `//` vengono letti come li legge il server.
- **Le connessioni SQL Server in sola lettura** rifiutano gli hint di lock e `WAITFOR`.
- **Le connessioni in sola lettura** rifiutano di avanzare una sequenza (`NEXT VALUE FOR`, `.NEXTVAL`).
- **Rinomina:**
  - Su tutti i motori viene rifiutato un dipendente il cui codice verrebbe diviso in più di un'istruzione.
  - I corpi delle routine Snowflake che contengono `$$` vengono mantenuti interi.
  - I nomi OrientDB che DBine non potrebbe racchiudere tra virgolette in modo sicuro vengono rifiutati.
- **Esportazione SQL:** i valori di tipo stringa vengono scritti nel modo in cui li legge il motore di origine, così tornano esatti quando esegui lo script lì. Inoltre non possono aggiungere istruzioni su un motore che legge i backslash in modo diverso.
  - **PostgreSQL, SQL Server, Oracle, SQLite, DuckDB, Firebird, SAP HANA, Trino, Db2, Teradata, Vertica, Exasol, Netezza e Dremio:** il backslash viene scritto con la funzione di caratteri del motore stesso, quindi nessun backslash compare dentro una stringa. Gli script Firebird contengono una riga per `INSERT`, che è tutto ciò che Firebird accetta.
  - **Motori che interpretano gli escape con backslash** (MySQL, ClickHouse, Snowflake, Redshift…): i backslash e gli apici vengono escapati nel modo in cui questi motori li leggono.
  - **Origini sconosciute** (una griglia di più database) ricevono una forma sicura su qualsiasi motore. La nuova opzione **Destinazione con stringhe standard** scrive i loro backslash così come sono.
- **Esportazione CSV e TSV:** la protezione dalle formule copre anche il testo dopo una `,` o un `;`, che un foglio di calcolo con un altro separatore di elenco dividerebbe in una cella a sé.
- **Rinomina su Snowflake:** le viste, le funzioni e le procedure che nominano l'oggetto rinominato vengono elencate perché tu le corregga a mano invece di essere ricreate, così mantengono il proprietario, `EXECUTE AS` e `SECURE`.

## [0.1.9] - 2026-10-09

### Novità
- **Rinominare con impatto:** **Rinomina…** nell'esplora risorse cambia il nome di una tabella, vista, routine, colonna, indice o schema e, nello stesso script, riscrive le viste, procedure, funzioni e trigger che lo usano. Prima di eseguire mostra cosa aggiorna da solo il motore, cosa viene riscritto e cosa va controllato a mano (SQL dinamico, codice illeggibile), insieme allo script completo. Viene eseguito in una transazione dove il motore lo consente. È disponibile in tutti i motori che possono rinominare qualcosa; i limiti di ciascuno sono in `docs/engine-support.md`.
- **Modificare una tabella:** **Modifica…** apre il designer su una tabella esistente e costruisce l'`ALTER` del motore. Conserva ciò che il designer non mostra (CHECK, opzioni degli indici, ordine delle colonne della chiave) e ricrea le viste e i trigger che dipendono dalla tabella. Rinominare una colonna lì passa dalla revisione dell'impatto; sulle connessioni di produzione chiede di digitare il nome della tabella prima di eseguire.
- **Cronologia per query:** la barra **Cronologia** segue la scheda attiva, come una linea temporale: versioni della query salvata con differenze e ripristino, le sue esecuzioni e, nei file di un progetto, i suoi commit git.
- **Navigazione nell'editor:** Cmd/Ctrl+clic su una tabella, vista o routine ne apre la struttura o la definizione, e **Mostra in esplora risorse** la individua nell'albero. Le tabelle e le colonne che non esistono vengono segnalate prima di eseguire.
- **Parametri nelle query:** `:nome` e `?` vengono richiesti all'esecuzione, e l'ultimo valore viene ricordato.
- **Snippet** per motore (per esempio, `sel` + Tab) e menu del tasto destro nell'editor.
- **Totali della selezione:** selezionando celle della griglia vengono mostrati conteggio, somma, media, minimo e massimo.
- **Attività pianificate:** script, esportazioni, confronto di schemi, backup, **Documenta il database** e **Invia un'email** (SMTP) che vengono eseguiti con DBine chiuso, tramite l'utilità di pianificazione del sistema. Con notifiche per attività e cronologia delle esecuzioni. Ciò che modifica i dati viene approvato esplicitamente.
- **Qualità del codice:** regole per motore nell'editor e **Vedi problemi**.
- **Documenta il database:** dizionario dei dati in HTML o Markdown, con diagramma, righe stimate e commenti di viste e routine. Le righe stimate e i commenti provengono dai metadati del motore, senza leggere le tabelle né consumare quota nei motori cloud.
- **Progetta query:** costruttore visuale di query.
- **Copia un sottoinsieme** di dati, con mascheramento.
- **Ottimizza query:** riscritture, indici suggeriti, alternative dell'IA e confronto misurato. Le alternative dell'IA vengono convalidate rispetto al piano stimato del database prima di essere mostrate.
- **Controllo di integrità** di un database, in tutti i motori, con verifiche proprie in SQL Server, nella famiglia PostgreSQL, nella famiglia MySQL, in Oracle, SAP HANA, Firebird, ClickHouse, Snowflake, BigQuery, Databricks e nei profili ODBC.
- **Cerca nel database:** nomi di oggetti, codice di viste e routine, e nomi di colonne (con la loro tabella e il tipo).
- **Genera dati di test** per una tabella.
- **Proprietà del database** e opzioni avanzate alla creazione di un database, in schede e per motore, con anteprima dello script.
- **Vista JSON ad albero** dei risultati, con modifica, e **Aggiungi riga** / **Aggiungi documento** nella scheda Dati e nella griglia.
- **Nuovo marchio:** il logotipo con l'alone.

### Miglioramenti
- Il colore della connessione appare come una striscia sul bordo della riga, e il punto indica solo lo stato (verde connessa, rosso disconnessa).

### Correzioni
- Il confronto degli schemi non viene più annullato dalle letture dell'esplora risorse, e SQL Server si riconnette.
- Le righe di connessione senza colore restano allineate con quelle che ce l'hanno.
- Trascinare le tabelle nel costruttore di query funziona su macOS.
- Proprietà di SQL Server: i nomi di file lunghi non escono più dalla finestra di dialogo, e la scheda «Opzioni ANSI e di sicurezza» è tradotta.
- L'editor non segnala più come sconosciute le colonne di una sottoquery con alias.

### Già disponibile
- **Eseguire una query su più database contemporaneamente:** si scelgono uno o più database di una connessione, e i risultati vengono uniti con una colonna che indica il database di ogni riga. È arrivato nella 0.1.4. Vedi `docs/multi-database-queries.md`.

## [0.1.8] - 2026-10-06

### Novità
- **PostgreSQL dietro gateway che accettano solo il protocollo semplice:** la connessione ha una nuova opzione, **Protocollo di query**: Automatico o Solo protocollo semplice. Serve per i gateway che rifiutano il protocollo esteso con l'errore 0A000. In quella modalità, ciò che richiede il protocollo esteso avvisa con un messaggio chiaro invece di fallire.

### Correzioni
- **Nuova query con la scheda Nuova connessione aperta** falliva con «FOREIGN KEY constraint failed». Ora la query si apre sull'ultima connessione che avevi aperto, oppure ti chiede di scegliere un database nell'esplora risorse.

## [0.1.7] - 2026-10-05

### Correzioni
- **Confrontare dati con colonne identity:** sincronizzare righe verso una tabella di SQL Server con una colonna `IDENTITY` falliva con «Cannot insert explicit value for identity column». Ora DBine attiva `IDENTITY_INSERT` solo mentre inserisce quelle righe.
- In PostgreSQL, dopo aver copiato righe con i loro id, la sequenza avanza in modo che il prossimo insert non si scontri con un id copiato.

## [0.1.6] - 2026-10-04

### Novità
- **Processi nel Monitor:** accanto al pannello, la scheda **Processi** elenca le sessioni e le query in corso del server, con filtri. Da lì si annulla una query o si termina una sessione. Disponibile in tutti i motori che lo espongono: SQL Server, PostgreSQL, MySQL, Oracle, MongoDB, Redis e la maggior parte degli altri.
- **Autenticazione di Windows in SQL Server:** con l'utente corrente (SSPI su Windows, Kerberos su macOS e Linux) o con utente e password di dominio, anche da Mac e Linux.
- **Kerberos in MongoDB.**

### Miglioramenti
- In ODBC, gli attributi extra della connessione sostituiscono quelli del modello.
- L'assistente IA ha una propria icona e non viene più confuso con **Formatta**.
- Le query eseguite dall'assistente vengono mostrate tradotte in tutte le lingue.
- La telemetria anonima conta anche l'uso dell'assistente, del server MCP, delle sincronizzazioni, delle migrazioni e delle query su più database. Mai nomi, query né dati; si disattiva in Impostazioni › Generale.

## [0.1.5] - 2026-10-03

### Novità
- **L'assistente IA legge il tuo database, con la tua approvazione:** con un modello locale può consultare la struttura e l'uso degli indici della connessione (per esempio, «analizza gli indici e dimmi quale è di troppo»). Prima di leggere righe o eseguire una query ti mostra l'SQL esatto e il database, con Approva o Rifiuta. Non modifica mai dati né struttura.

### Miglioramenti
- **Ferma** interrompe la risposta dell'assistente in qualsiasi momento e **Nuova conversazione** è sempre disponibile.
- L'opzione «struttura» della chat non serve più: l'assistente chiede i dettagli quando gli servono.
- Il cursore di testo appare dove si può selezionare o scrivere.

## [0.1.4] - 2026-10-03

### Novità
- **Progetti:** repository Git di SQL collegati alle tue connessioni, dalla seconda icona della barra laterale. Albero dei file, database attivo o ambienti (dev/qa/prod) senza credenziali nel repository, modifiche con diff, commit, pull e push. Ogni database mostra in Esplora risorse i progetti collegati.
- **Eseguire una query su più database contemporaneamente:** la stessa query su più database di una connessione, con i risultati insieme e una colonna che indica il database.
- **DBine si aggiorna da solo:** scarica la nuova versione, verifica la firma e si riavvia (chiede prima se ci sono attività in background). La 0.1.4 si installa a mano per l'ultima volta. Su Linux funziona con l'AppImage; con .deb/.rpm continua a offrire il download.
- **Disabilitare e abilitare indici** dall'esplora risorse e dalla scheda Indici, nei motori che lo consentono (SQL Server, MySQL, MariaDB, TiDB, Oracle, Firebird, CockroachDB, MongoDB…).
- **Selezione di celle nella griglia:** un blocco (trascinando, Shift+clic o Shift+frecce) per copiarlo, oppure celle e righe non contigue con Cmd/Ctrl+clic.
- **Confronto di schemi:** può eliminare un elemento a sinistra, a destra o su entrambi i lati e, prima di eseguire, mostra cosa ne dipende.

### Miglioramenti
- **Assistente IA:** consiglia un modello integrato più grande in base alla memoria del tuo computer, conosce le particolarità di ogni dialetto, riprova se si rifiuta di rispondere e conserva la cronologia delle conversazioni in un pannello.
- Il pannello delle Attività ha **Rimuovi completate** in alto e si chiude con Esc o con un clic all'esterno.
- Azure SQL Database (anche Hyperscale): connesso a master, elenca tutti i database del server.
- CockroachDB: gli indici appaiono come BTREE e GIN, come in PostgreSQL.
- Il testo della chat IA può essere selezionato e copiato.

### Correzioni
- La barra della query non si scompone più all'apertura del pannello IA.
- La sincronizzazione degli schemi di libSQL non fallisce più per un'istruzione `PRAGMA` che il server rifiuta.

## [0.1.3] - 2026-10-02

### Novità
- **Più finestre** nella stessa istanza: **Nuova finestra** dal Dock, dalla barra delle applicazioni, File › Nuova finestra o Cmd/Ctrl+Shift+N. Connessioni, query salvate e impostazioni sono condivise tra le finestre.
- **Attività in background:** le operazioni lunghe (sincronizzare schemi o dati, backup, generare script, importare, esportare, clonare tabelle, eliminare oggetti) possono continuare in background. Il pannello delle Attività mostra avanzamento, tempo trascorso, tempo rimanente stimato e Annulla, e avvisa al termine. Alla chiusura dell'applicazione con attività in corso, chiede conferma prima di annullarle.
- **Uso degli indici** in tutti i motori che lo riportano: chiavi PK e FK sulle colonne, cartella Indici, percentuale di letture per indice con un colore in base a seek e scan, ed eliminazione di un indice dall'esplora risorse.
- **Mostra dipendenze…:** cosa dipende da una tabella, colonna, vista o routine.

### Miglioramenti
- La sincronizzazione dei dati applica ogni lato in una sola transazione.
- Completamento automatico SQL dopo «schema.» e «tabella.».
- Il confronto di schemi sincronizza i commenti, ha frecce reversibili e un elenco ridimensionabile.

### Correzioni
- La sincronizzazione degli schemi elimina le chiavi esterne duplicate una per una e, in SQL Server, cambia in modo sicuro l'indice clustered di una tabella.

## [0.1.2] - 2026-10-01

### Novità
- **Esecuzione di script come nello strumento di ciascun motore:** istruzione per istruzione, con `GO` / `GO N`, `DELIMITER`, `/` e `SET TERM`. Opzione **Continua in caso di errore**, messaggi in tempo reale ordinati, errori con codice e riga, ed esecuzione dell'istruzione al cursore.
- **Transazioni Auto/Manuale** con Conferma e Annulla, e conferma prima di un UPDATE o DELETE senza WHERE.
- **Schemi:** creare ed eliminare schemi con proprietario e permessi; gli schemi vuoti compaiono nell'esplora risorse.
- **Avviso di nuova versione:** DBine avvisa quando c'è una nuova versione, all'apertura e da Aiuto › Verifica aggiornamenti….
- Riordinare connessioni e cartelle trascinando.
- Eliminare righe dalla griglia dei dati e salvare le modifiche con Cmd/Ctrl+S.

### Miglioramenti
- Annullare una query mantiene la sessione.

### Correzioni
- Il driver di Solr è stato ripubblicato (condivide codice con quello di Elasticsearch).

## [0.1.1] - 2026-09-30

### Novità
- **Telemetria anonima**, attiva per impostazione predefinita, con un avviso la prima volta. Si disattiva in Impostazioni o con `DO_NOT_TRACK` / `DBINE_TELEMETRY=0`.
- PostgreSQL: opzioni di identità (colonne identity) nella progettazione delle tabelle.

### Miglioramenti
- Oracle: le definizioni includono gli indici.
- Scheda di definizione completa, con messaggi di errore di migrazione più chiari.
- Il confronto di schemi conserva le righe e sincronizza in un solo passaggio.
- La scheda di confronto dei dati ricorda le tue selezioni.
- Driver di PostgreSQL e Oracle aggiornati alla 0.1.2.

## [0.1.0] - 2026-09-30

### Novità
- Prima versione di DBine, con installer per Windows, macOS (Apple Silicon e Intel) e Linux. I driver di ciascun motore, tranne SQLite, vengono scaricati la prima volta che ti connetti.
