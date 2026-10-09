# Modifiche

## [0.1.10] - 2026-10-09

### Novità
- **I driver si aggiornano da soli, separatamente dall'app:** DBine cerca in un indice firmato il driver più recente compatibile con la tua versione, lo scarica in background e torna al precedente se qualcosa va storto. In Impostazioni › Driver c'è un pulsante **Cerca aggiornamenti**, lo stato di ogni driver e **Torna alla precedente**. Un driver può essere pubblicato da solo, senza una nuova versione dell'app.
- **Rinominare un database:** **Rinomina…** su un database nell'esplora risorse. La finestra mostra le altre sessioni aperte su di esso (che la ridenominazione termina), se il nuovo nome esiste già, quanti oggetti vengono spostati e lo script completo. Dopo, il database predefinito della connessione, le schede aperte, le query salvate, le migrazioni, le destinazioni dei progetti e i passaggi delle attività pianificate seguono il nuovo nome; le attività che modificano dati al suo interno chiedono di nuovo l'approvazione. Disponibile in SQL Server, Azure SQL, Babelfish, nella famiglia PostgreSQL, in MySQL, MariaDB, Snowflake e MongoDB; dove il motore non può rinominare (o spostare) un database, non viene offerto.
- **A quale colonna corrisponde questo valore?** In un `INSERT … VALUES`, posizionando il cursore su un valore compare un tooltip con la sua colonna (ad esempio "Colonna 14 di 48: Name") e quella colonna viene evidenziata nell'elenco. Senza elenco di colonne, usa quelle della tabella in ordine. Funziona su tutti i motori SQL e CQL.

### Miglioramenti
- **Cosa porta ogni versione:** l'avviso di nuova versione mostra le sue modifiche e quelle delle versioni intermedie, a partire da quella che hai installato, nella lingua dell'app.
- **Versioni vecchie dell'app:** d'ora in poi, un'app più vecchia delle ultime cinque versioni deve aggiornarsi per scaricare nuovi driver. I driver che ha già installati continuano a funzionare.

### Correzioni
- **La sola lettura è più rigorosa:** le query dei client MCP con livello Lettura, dell'assistente IA e delle connessioni in sola lettura vengono controllate parola per parola, non solo dalla prima. Ora rifiutano:
  - una scrittura nascosta dopo una lettura in un batch di SQL Server;
  - un `WITH` che modifica i dati;
  - `SELECT … INTO`, `EXEC` e `SET`;
  - funzioni che agiscono al di fuori della query, come `set_config`, `dblink_exec` e `xp_cmdshell`.

  Un piano stimato rifiuta gli script che disattiverebbero la modalità piano, quindi le alternative IA di **Ottimizza query** non possono eseguire nulla. Le attività pianificate i cui script ora contano come scrittura chiedono di nuovo l'approvazione.
- **Libreria con git:** un repository condiviso non può più far leggere, scrivere o eliminare a DBine file al di fuori della cartella della Libreria.
- **Esportazione SQL:** i valori di tipo stringa vengono escapati nel modo in cui li legge il motore di origine, così un valore memorizzato non può aggiungere istruzioni a uno script `INSERT` per MySQL, ClickHouse, BigQuery, Hive, Spark o Databricks.
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
