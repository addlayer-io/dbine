# Modifiche

## [1.1.0] - 2026-10-21

### Novità
- **Stato della sincronizzazione cloud a colpo d'occhio:** la barra di stato mostra **Sincronizzato** con un punto verde, **In attesa** in ambra mentre una modifica aspetta di essere caricata, e rosso in caso di errore; passa il mouse per vedere l'ora dell'ultima sincronizzazione e fai clic per aprire le impostazioni di sincronizzazione.
- **Copia stringa di connessione:** fai clic con il tasto destro su una connessione (o usa il pulsante nel suo modulo) e scegli **Copia stringa di connessione…** per ottenerla come stringa URI, ADO.NET, JDBC, SQLAlchemy, ODBC, libpq, Node.js o Go, quelle che si applicano al suo motore, per qualsiasi database, con le sue impostazioni TLS e di accesso. La password è esclusa (un segnaposto `${DB_PASSWORD}`) a meno che tu non attivi **Includi password**; in tal caso viene recuperata al momento della copia e cancellata dagli appunti un minuto dopo, se è ancora lì.
- **Passaggio Invia un messaggio** nelle attività pianificate: pubblica l'esito dell'esecuzione su Microsoft Teams, Slack, Telegram o ntfy, con le variabili dell'attività (`{results}` elenca cosa ha fatto ogni passaggio), **Solo se…** come nel passaggio email e un pulsante **Prova**. L'URL del webhook e i token restano nel portachiavi, mai nell'attività, e i messaggi viaggiano solo via HTTPS verso i server del servizio stesso.

### Correzioni
- **Tunnel SSH, sicurezza:** la libreria SSH è aggiornata per correggere due avvisi (CVE-2026-111061, un denial of service tramite l'agente SSH, e CVE-2026-111062).
- **Chiavi RSA, sicurezza:** l'accesso SSH con una chiave RSA e l'apertura delle chiavi dell'organizzazione di Zoho Vault e Bitwarden ora vengono eseguiti in AWS-LC in tempo costante, quindi la fuga di informazioni dai tempi della libreria RSA (RUSTSEC-2023-0071, "Marvin") non si applica più. Le chiavi RSA firmano con SHA-256 o SHA-512; un server che accetta solo SHA-1 (`ssh-rsa`) richiede una chiave Ed25519 o ECDSA, oppure l'agente SSH.
- **TLS, sicurezza:** la libreria TLS e i suoi controlli dei certificati sono aggiornati in DBine e in 33 driver (RUSTSEC-2026-0346 a 0349 e 0381 a 0383), così i certificati di autorità non più attendibili vengono rifiutati. I driver aggiornati si scaricano da soli.
- **Motore di IA integrato:** nel decomprimere il motore vengono mantenuti solo i permessi di lettura, scrittura ed esecuzione dei file.
- **Attività pianificate, sicurezza:** `{results}` e `{alerts}` riempiono solo il testo di email e messaggi. L'SQL di uno script, il nome di un file o di una cartella, un indirizzo o il percorso di un allegato non li accettano più, così un errore del database che riporta dati manipolati non può trasformarsi in SQL né in un percorso.
- **Messaggi delle attività pianificate:** `{results}` e `{alerts}` in un messaggio o in un'email escono nella lingua dell'interfaccia di DBine invece che sempre in spagnolo, anche quando l'attività viene eseguita con DBine chiuso.
- **Notebook, risultati grandi di DataFrame SQL:** **Esportazione avanzata… › Tutte** scrive il risultato completo, non solo le righe mostrate. L'SQL della cella viene rieseguito in DuckDB sui DataFrame letti dalla sua ultima esecuzione, senza rileggere file né interrogare di nuovo le sorgenti. I suoi grafici partono già calcolati sull'intero risultato, e si può disattivare. Quando un risultato è troncato, la griglia avvisa anche che l'ordinamento e **Copia tutto** riguardano solo le righe caricate.

## [1.0.0] - 2026-10-19

### Novità
- **Tema chiaro:** un tema chiaro e un'opzione **Come il sistema** (quella predefinita) che segue il tuo sistema operativo quando cambia; zoom dell'interfaccia con Cmd/Ctrl + «+», «−», «0» o la rotellina del mouse, e dimensioni del carattere separate per l'editor e per la griglia dei risultati, in **Impostazioni › Generale**.
- **Modalità Vim e scorciatoie personalizzate:** modalità Vim per l'editor SQL (Impostazioni › Generale), e ogni scorciatoia da tastiera si può cambiare in **Impostazioni › Scorciatoie da tastiera**, con ricerca, ripristino e avvisi di conflitto.
- **Editor diviso:** fino a tre gruppi di editor affiancati o uno sopra l'altro (il menu della scheda, il pulsante della barra delle schede o Cmd/Ctrl+\); trascina le schede tra i gruppi o sulla metà destra o inferiore di un gruppo per dividerlo. Il layout viene ripristinato al riavvio.
- **Accesso cloud per i database gestiti:** le connessioni PostgreSQL e MySQL verso Amazon RDS e Aurora, Google Cloud SQL e AlloyDB, e Azure Database possono accedere con **AWS IAM**, **Google Cloud IAM** o **Microsoft Entra ID**. DBine genera un token nuovo dal tuo profilo AWS (SSO incluso), da gcloud o da az prima di ogni connessione: niente più token da incollare che scadevano dopo 15 minuti, e **Prova** mostra con quale identità è avvenuto l'accesso.
- **Password da un comando o da un gestore di segreti:** qualsiasi campo segreto di qualsiasi motore, e la password e la passphrase della chiave del tunnel SSH, possono prendere il proprio valore da un comando, 1Password, HashiCorp Vault, AWS Secrets Manager, Azure Key Vault o Google Secret Manager. Viene ottenuto a ogni connessione e conservato solo in memoria, con un'opzione per la chiave JSON e una prova per campo che mostra solo la lunghezza. Un comando o un gestore di segreti proveniente da un'importazione o da un backup ripristinato viene eseguito solo dopo la tua conferma su questo computer.
- **KeePass e Zoho Vault come fonti di password:** qualsiasi campo segreto (e quello del tunnel SSH) può prendere il proprio valore da una voce di un database KeePass (.kdbx, letto da DBine stesso; password principale dal portachiavi, chiesta alla connessione o ottenuta da un comando; file chiave facoltativo) o da un segreto di Zoho Vault (qualsiasi data center, decifrato sul tuo computer con la tua passphrase principale), e, se vuoi, l'utente della connessione viene dalla stessa voce.
- **Credenziali di database di Vault, CyberArk e Delinea come fonti di password:** le password delle connessioni possono arrivare dal motore di segreti dei database di HashiCorp Vault (credenziali create per la connessione, rinnovate finché è aperta e revocate alla chiusura; accesso AppRole anche per Vault), da CyberArk Conjur, dal Central Credential Provider di CyberArk (con certificati client) e da Delinea Secret Server.
- **Bitwarden, Bitwarden Secrets Manager e AWS Parameter Store come fonti di password:** qualsiasi campo segreto (e quello del tunnel SSH) può prendere il proprio valore da un elemento di Bitwarden, personale o di un'organizzazione, su bitwarden.com, bitwarden.eu o sul tuo server, decifrato sul tuo computer con la tua password principale; da un segreto di Bitwarden Secrets Manager (con il token di accesso di un account macchina); o da un parametro di AWS Systems Manager (SecureString, String o un elemento di una StringList), con una chiave JSON e, se vuoi, l'utente della connessione dallo stesso elemento.
- **Prima le credenziali, ed esplora il tuo gestore di segreti:** il modulo di connessione chiede prima le **Credenziali**: tenere la password nel portachiavi, chiederla ogni volta, oppure prendere utente e password da una riga di comando o da un gestore di segreti, i cui campi li sostituiscono. Un pulsante **Sfoglia** elenca le voci di un gestore e i nomi dei loro campi così puoi sceglierli invece di digitare i riferimenti; mostra solo nomi, mai valori.
- **Utente e password dalla voce:** con KeePass, Bitwarden, 1Password, Zoho Vault, Delinea o CyberArk CCP come credenziali, sia l'utente sia la password vengono dalla voce, e **Impostazioni avanzate** permette di scegliere altri campi dall'elenco della voce stessa. Ogni fonte chiede prima il proprio accesso e la voce per ultima, e Sfoglia è pronto quando questo è compilato.
- **Account Zoho Vault, collegati una volta sola:** un assistente guidato **Collega Zoho Vault** ti accompagna nella console di Zoho, trova il tuo data center e verifica la tua passphrase principale; poi qualsiasi connessione sceglie semplicemente l'account. Gli account restano su questo computer e si gestiscono in **Impostazioni › Gestori di segreti**.
- **Account per ogni gestore di segreti:** Bitwarden, Bitwarden Secrets Manager, HashiCorp Vault e OpenBao, Delinea, CyberArk Conjur e Central Credential Provider, 1Password, AWS, Azure Key Vault, Google Secret Manager e KeePass ora si collegano una volta sola in **Impostazioni › Gestori di segreti** con un assistente guidato che spiega dove si trova ogni valore, verifica l'account in tempo reale e mostra chi ha effettuato l'accesso; poi qualsiasi connessione sceglie semplicemente l'account, e le credenziali vanno solo al server proprio di quel gestore.
- **Tenant di Azure tra cui scegliere:** l'assistente degli account di Azure Key Vault elenca i tuoi tenant di Microsoft Entra (puoi comunque digitarne uno), e quando non hai effettuato l'accesso al tenant scelto mostra il comando `az login --tenant` da copiare.
- **Completamento automatico nelle celle DataFrame SQL:** le celle qui sopra vengono suggerite come tabelle (anche quelle non ancora eseguite), con le loro colonne dopo `nome.` o un alias e nell'elenco del select, e i lettori di file di DuckDB dopo FROM e JOIN; scegliendo un lettore, il cursore si posiziona tra le sue virgolette, dove **Scegli file…** apre la finestra dei file. L'elenco delle tabelle dopo FROM non si chiude più da solo mentre il notebook si aggiorna.
- **Vista mappa:** i risultati con colonne di geometria (PostGIS, MySQL, SQL Server, SQLite/SpatiaLite, DuckDB, MongoDB, Elasticsearch e altri) o colonne di latitudine/longitudine si possono mostrare su una mappa, disegnata sulla tua macchina sopra un mappamondo integrato che funziona offline. I tasselli di mappa online più dettagliati si possono attivare in **Impostazioni › Mappe**; il provider vede allora quale zona stai guardando, mai i dati.
- **Oracle sulla mappa:** le colonne SDO_GEOMETRY ora compaiono sulla mappa dei risultati, sia nei dati di una tabella sia nelle tue query (convertite sul server in WKT con il loro SRID); quelle celle sono di sola lettura nella griglia.
- **Apri direttamente i file di dati:** i file SQLite, DuckDB, CSV, TSV, Parquet, JSON ed Excel si aprono con un doppio clic o con «Apri con DBine», trascinandoli sulla finestra, o con **File › Apri file…** (Cmd/Ctrl+O). Ognuno si apre come una connessione temporanea che puoi conservare con **Salva connessione**, e i file di dati mostrano subito le prime 1.000 righe.
- **Tavolozza dei comandi:** Cmd/Ctrl+Shift+P trova ed esegue qualsiasi azione, mostrando solo ciò che il motore attuale supporta, e **Vai a…** (Cmd/Ctrl+P) salta a connessioni, database, tabelle, viste, routine, query salvate e schede aperte. A ogni azione della tavolozza si può anche assegnare una scorciatoia propria.
- **Salta alle righe correlate:** nei dati di una tabella o nei risultati di una query, **Vai alla riga referenziata** (o Cmd/Ctrl+clic) apre la tabella padre filtrata da quella chiave esterna, e **Righe che la referenziano** apre ogni tabella figlia filtrata dal valore, comprese le chiavi composte. Su tutti i motori con chiavi esterne.
- **Notebook SQL:** **File › Nuovo notebook** apre un notebook: celle SQL che si eseguono su qualsiasi connessione, ognuna con il proprio risultato e grafico sotto, e celle di testo per spiegarle. Viene salvato come un `.ipynb` Jupyter standard che GitHub, VS Code e Jupyter mostrano, e che aprono anche Azure Data Studio e i notebook `%%sql` di Jupyter. Il file conserva un'anteprima di ogni risultato (100 righe e 1 MB per cella, in **Impostazioni › Notebook**) con il totale delle righe, l'ora e la durata, mai i dettagli di connessione né le password; all'interno di un progetto, i notebook vengono salvati senza risultati a meno che tu non li attivi. L'assistente può spiegare un risultato, proporre la cella successiva o una versione raggruppata di una query grande, senza vedere le righe. Esporta in HTML o Markdown.
- **Grafici sull'intero risultato:** i grafici sono calcolati su tutte le righe del risultato, non solo su quelle caricate nella griglia. Sui motori SQL il raggruppamento avviene sul server, nelle celle DataFrame SQL di un notebook lo fa DuckDB, e sugli altri motori lo fa DBine.
- **Più tipi di grafico:** istogramma, mappa di calore, box plot (con baffi e valori anomali), barre con linee e KPI (il totale dell'intero risultato).
- **Più grafici per cella di notebook:** una cella di notebook può mostrare più di un grafico del suo risultato, e ricorda quale fonte usa la sua mappa.
- **Grafici nelle esportazioni dei notebook:** i notebook esportati in HTML o Markdown includono i loro grafici come immagini.
- **Unisci risultati di database diversi in un notebook:** il risultato di ogni cella è un DataFrame con il nome della cella (mostrato e modificabile nella sua intestazione), e una cella **DataFrame SQL** interroga e unisce i DataFrame qui sopra come tabelle con l'SQL di DuckDB e senza connessione, così una cella può unire un risultato di PostgreSQL con uno di SQL Server e uno di MongoDB. Viene eseguita in un database DuckDB in memoria che non può scrivere file né accedere alla rete, e viene eliminato alla chiusura del notebook; le righe di ogni fonte provengono dalla sua esecuzione in questa sessione o dalla sua query rieseguita, in sola lettura, sulla propria connessione (fino a 1.000.000 di righe per DataFrame, in **Impostazioni › Notebook**).
- **Notebook come report pianificati:** un'attività pianificata può eseguire un notebook con DBine chiuso (**Esegui notebook**), salvare la copia eseguita ed esportarla in HTML o Markdown, e il passaggio successivo **Invia un'email** può allegarla o inviare l'esportazione HTML come corpo dell'email. Non viene chiesto nulla in modo non presidiato: legge solo i file con **Consenti sempre** per quel notebook, viene eseguita sugli ambienti che chiedono conferma solo con il permesso esplicito del passaggio, e le sue celle che modificano dati rientrano nell'approvazione dell'attività. Le esportazioni pianificate includono le tabelle; i grafici sono inclusi quando si esporta dall'app.
- **File locali nelle celle DataFrame SQL:** una cella DataFrame SQL può leggere file CSV, Parquet, JSON o NDJSON ed Excel dal tuo computer con `read_csv`, `read_parquet`, `read_json` e `read_xlsx`, compresi pattern come `data/*.csv` ed elenchi di file, file geografici (GeoJSON, Shapefile con i suoi file complementari, GeoPackage, KML, GPX, FlatGeobuf) con `st_read`, e tabelle di file di database SQLite e DuckDB con `sqlite_scan` e `read_duckdb`, ciascuno come un DataFrame che si può unire ai risultati delle celle di qualsiasi motore; le geometrie continuano a funzionare con funzioni spaziali come `ST_Contains` e compaiono sulla mappa. I file vengono solo letti, mai scritti. Prima di leggere un file per la prima volta, il notebook chiede: consentirlo per questa esecuzione, sempre per questo notebook su questo computer (un pattern copre i suoi file nuovi) o negarlo; credenziali, chiavi, cronologia della shell e file di sistema non vengono mai letti. Se il contenuto di un file è cambiato dall'ultima esecuzione, il risultato lo indica. Impostazioni › Notebook elenca i file che ogni notebook può leggere e permette di rimuoverli. Gli stessi lettori leggono anche file dal web (`https://`) e da Amazon S3, Google Cloud Storage e Azure Blob Storage (`s3://`, `gs://`, `az://`, `abfss://`, con pattern come `s3://bucket/2026/*.parquet`), con la stessa approvazione, che mostra prima il server; DBine li scarica da sé in una cartella temporanea privata che viene rimossa dopo la lettura, usando l'accesso AWS, Google Cloud o Azure di questo computer (in Impostazioni › Notebook si può scegliere un profilo AWS), e non raggiunge mai indirizzi locali o di reti private, non segue reindirizzamenti verso altri server né accetta credenziali nell'URL. Un GeoPackage aperto in un altro programma ora include le modifiche ancora presenti nel suo `-wal`.
- **Esportare un notebook come notebook Python:** **Esporta › Notebook Python (.ipynb)** scrive un notebook Jupyter che lo riproduce: un engine SQLAlchemy (o il client Python proprio del motore, come pymongo o redis) per connessione, ogni cella di query letta in un DataFrame pandas con il nome della cella, le celle DataFrame SQL eseguite con DuckDB su quei DataFrame, e le celle di testo come Markdown. Password e token non vengono mai esportati: il codice li legge da variabili d'ambiente, che la prima cella elenca insieme ai pacchetti da installare. I risultati sono esclusi a meno che tu non scelga di includere le anteprime.
- **Scrivere un DataFrame in una tabella da un notebook:** una **cella di scrittura** (aggiunta sotto qualsiasi cella con **Scrivi in una tabella…**) scrive l'intero risultato di una cella in una tabella di SQL Server, PostgreSQL, MySQL, MariaDB, Oracle, SQLite, DuckDB, MongoDB e altri: inserire, aggiornare per chiave, inserire o aggiornare, o sostituire il contenuto della tabella, con una mappatura delle colonne, un'anteprima e una nuova tabella se vuoi. Ogni scrittura chiede prima conferma, mostrando dove, come e quante righe (le connessioni di produzione chiedono di digitare il nome della tabella), viene eseguita in una sola transazione sui motori che le hanno, e le connessioni di sola lettura non vengono mai scritte. In un'attività pianificata, una cella di scrittura viene eseguita solo quando il passaggio autorizza quella cella, tabella e modalità.
- **Parametri del notebook:** un notebook può definire parametri (testo, numero, data, data e ora, booleano o un elenco fisso, con un valore predefinito) che le sue celle usano come `{{name}}`; all'esecuzione vengono chiesti i valori mancanti, ogni valore arriva al database come dato (associato dal driver o racchiuso tra apici secondo le regole proprie del motore), e ogni risultato mostra i valori che lo hanno prodotto.
- **Altre cinque lingue:** DBine e la sua app per il telefono ora parlano tedesco, russo, cinese semplificato, giapponese e coreano, oltre a spagnolo, inglese, portoghese, francese e italiano; la lingua segue quella del tuo sistema e viene caricata solo quella in uso.

### Miglioramenti
- **Firmata su macOS:** l'app per macOS è firmata con un Apple Developer ID e notarizzata da Apple, quindi si apre senza l'avviso «impossibile verificare lo sviluppatore» né alcun comando da terminale. La prima volta che si apre la versione firmata, macOS chiede una sola volta se DBine può usare la propria chiave nel portachiavi: scegli **Consenti sempre**.
- **L'accesso cloud si fida solo del certificato dell'istanza:** le connessioni che accedono con AWS IAM, Google Cloud IAM o Microsoft Entra, e le connessioni a Cloud SQL e AlloyDB con il file CA dell'istanza, ora accettano solo quella CA (o il bundle RDS di Amazon), non qualsiasi certificato pubblico, prima di inviare un token o una password. Un server che chiede la password in chiaro mentre **Considera attendibile il certificato del server** è attivo ora fallisce invece di riceverla.
- **Firmata su Windows:** il programma di installazione di Windows e l'app sono firmati con un certificato di firma del codice, quindi Windows mostra chi pubblica DBine invece di «Autore sconosciuto».

### Correzioni
- **Monitor del server:** le sue cifre, tabelle e note sono tradotte in tutti i motori, invece di mostrare alcune etichette in spagnolo.
- **Assistente IA e ottimizzatore:** le istruzioni al modello sono nella lingua dell'interfaccia, così un modello locale risponde in quella lingua invece che in spagnolo. Anche la riga che indica cosa è stato inviato al modello è tradotta.

## [0.1.14] - 2026-10-17

### Novità
- **DBine sul tuo telefono (Android):** in **Impostazioni › Dispositivi**, attiva **Consenti telefoni** e associa l'app DBine del telefono scansionando un codice QR; poi conferma sul computer che entrambi gli schermi mostrino lo stesso codice. Dal telefono vedi il Monitor di ogni connessione, chi blocca chi, le sessioni in corso e il controllo di salute di un database; in **Attività**, le query che esegui nell'editor di DBine e le attività pianificate, con il loro dettaglio. Se consenti quel telefono, terminare una sessione, annullare una query, o eseguire o annullare un'attività richiede ogni volta la tua impronta, il volto o il PIN. Il telefono ti avvisa quando termina una query lunga e quando un'attività fallisce o ha un allarme, anche con l'app chiusa (ogni 15 minuti). Le tue connessioni e password non escono dal computer: il telefono chiede a DBine tramite un canale cifrato end-to-end.
- **Accesso del telefono da fuori della tua rete:** disattivato per impostazione predefinita (solo rete locale). Attivalo in **Impostazioni › Dispositivi › Fuori dalla rete locale**, tramite il tuo server di relay o i server pubblici di iroh, che vedono gli indirizzi IP ma mai il contenuto. Come è protetto: [dbine.com/it/phone-security](https://dbine.com/it/phone-security).
- **Le query lunghe tengono sveglio il computer:** finché una query dell'editor è in esecuzione da 30 secondi o più, DBine impedisce al computer di andare in stop, così la connessione non cade.
- **Download dell'app del telefono:** l'app del telefono (Android) viene pubblicata a ogni versione e su dbine.com.

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
