# Modifications

## [1.2.0] - 2026-10-27

### Nouveautés
- **Ukrainien et polonais :** DBine et l'application du téléphone sont disponibles en Українська et Polski, messages du serveur et assistant IA compris. Choisissez la langue dans **Paramètres › Général › Langue**, ou DBine suit celle du système.
- **Indonésien :** DBine et l'application du téléphone sont disponibles en Bahasa Indonesia, messages du serveur et assistant IA compris. Choisissez la langue dans **Paramètres › Général › Langue**, ou DBine suit celle du système.

### Améliorations
- **Complétion entre bases de données :** pour les moteurs où une requête peut lire une autre base de la même connexion (ClickHouse, MySQL et MariaDB, SQL Server, Snowflake, Databricks, Trino, BigQuery, Oracle et d'autres), l'éditeur propose les noms des autres bases, leurs tables après `autre_bd.` et leurs colonnes après une table ou un alias, y compris dans les jointures entre bases. Les tables de chaque base sont lues la première fois que vous tapez son nom suivi d'un point.
- **Moteur DuckDB mis à jour en 2.0.0 :** les fichiers DuckDB locaux et le DataFrame SQL des notebooks s'exécutent désormais sur DuckDB 2.0.0 (contre 1.5.5). Les nouveaux fichiers de base utilisent le format de stockage de DuckDB 2.0, que les versions antérieures de DuckDB ne peuvent pas ouvrir ; les fichiers existants continuent de s'ouvrir et restent lisibles par la version qui les a créés.
- **Chiffrement de la connexion pour SQL Server, PostgreSQL et MySQL :** le formulaire de connexion affiche les options TLS propres à chaque moteur, avec les noms qu'utilisent ses outils : `Encrypt` (Optional, Mandatory, Strict), `TrustServerCertificate`, `ServerCertificate` et `HostNameInCertificate` pour SQL Server et Azure SQL ; `sslmode` et `sslrootcert` pour PostgreSQL et sa famille ; `ssl-mode` et `ssl-ca` pour MySQL et sa famille ; `ssl`, `ssl-verify-server-cert` et `ssl-ca` pour MariaDB et OceanBase. Les nouvelles connexions sont chiffrées et vérifient le certificat du serveur, et chaque option peut être désactivée. Les connexions enregistrées continuent de fonctionner exactement comme avant. Via un tunnel SSH, le certificat est vérifié par rapport au nom du serveur réel, et un échec de certificat indique l'option à modifier. Le `PREFERRED` de MySQL bascule vers une connexion non chiffrée uniquement si le serveur n'offre pas TLS, avant l'envoi de tout mot de passe.
- **Chiffrement de la connexion pour InfluxDB, IoTDB, ksqlDB, TDengine, Phoenix, Arrow Flight SQL et les services cloud :** le formulaire de connexion affiche les options TLS propres à chaque moteur : `ssl` et `unsafeSsl` pour InfluxDB 1, le schéma de l'URL avec `skip-verify` (InfluxDB 2) ou `tls-no-verify` et `tls-ca` (InfluxDB 3), `use_ssl` et `ca_certs` pour IoTDB et TimechoDB, `useTls`, `verifyHost` et `trustStore` pour ksqlDB, `useSSL` pour TDengine, le schéma de l'URL, `hostname_verification` et `truststore` pour Phoenix et Avatica, `useEncryption` et `tlsRootCerts` pour Flight SQL, et `DisableServerCertificateValidation` pour l'émulateur Cosmos DB ; lorsque les outils d'un moteur n'ont pas de nom pour ignorer la vérification ou pour le fichier de l'AC, `insecure` et `cacert` de curl. Les nouvelles connexions sont chiffrées et vérifient le certificat du serveur, et chaque option peut être désactivée. Les connexions enregistrées continuent de fonctionner exactement comme avant. Via un tunnel SSH, le certificat est vérifié par rapport au nom du serveur réel, et un échec de certificat ou un serveur sans TLS est signalé comme tel. BigQuery, Spanner, DynamoDB, Athena, Snowflake, Databricks et libSQL utilisent toujours HTTPS vérifié, sauf pour un endpoint d'émulateur saisi en `http://`.
- **Clavier et lecteurs d'écran :** un anneau de focus visible montre où se trouve le clavier ; les lignes de listes, les cartes, les en-têtes triables et les sections repliables sont accessibles avec Tab et s'utilisent avec Entrée ou Espace ; les menus contextuels fonctionnent avec les flèches ; les boutons à icône seule ont des noms lus par un lecteur d'écran ; le résultat d'une requête (terminée, ou en échec avec son erreur) est annoncé ; les animations s'arrêtent lorsque le système demande une réduction des mouvements ; et le texte secondaire a plus de contraste dans les deux thèmes.

### Corrections
- **Les nouvelles bases apparaissent sans reconnexion :** une base créée avec `CREATE DATABASE` dans une requête (ou supprimée, ou renommée) apparaît aussitôt dans l'explorateur et les listes de bases, et l'ouverture d'une liste de bases (migration, comparaison, subset, sauvegardes, permissions, le sélecteur de base de la requête…) la relit, de sorte qu'une base créée depuis un autre outil apparaît aussi.
- **Import depuis SQL Server Management Studio :** en plus des serveurs enregistrés, DBine importe désormais les serveurs dont se souvient la boîte de dialogue Connect de SSMS (son historique de connexions), avec leur identifiant, leur base et leurs paramètres de chiffrement, de SSMS 18 à 20 et les nouveaux dossiers de SSMS 21 et suivants. Sur macOS et Linux, la boîte de dialogue de fichiers accepte désormais les fichiers `RegSrvr.xml`, `UserSettings.xml` et `.regsrvr` (ainsi que le `dataSources.xml` de DataGrip) ; auparavant, seuls les fichiers JSON pouvaient être choisis.
- **L'import de chaînes de connexion conserve leur chiffrement :** une chaîne collée ou importée apporte désormais ses options TLS sous la forme propre au moteur : `Encrypt` de SQL Server (y compris `Strict`) et `HostNameInCertificate`, le `sslmode` de PostgreSQL tel qu'écrit (`require` ne devient plus une connexion vérifiée), le `ssl-mode` de MySQL et le `useSSL` de Connector/J, le `sslMode` de MariaDB, et les schémas `rediss://`, `bolt+s://` et `bolt+ssc://`. Une chaîne sans paramètres TLS est importée comme son propre client se connecterait. Il en va de même pour les connexions importées depuis DBeaver, DataGrip et les autres IDE JetBrains, DbGate et Azure Data Studio.
- **Erreurs de certificat sur macOS et Windows :** un certificat de serveur auquel le système ne fait pas confiance (« The certificate was not trusted », « issued by an authority that is not trusted », « The target principal name is incorrect ») est désormais reconnu comme tel, et le message indique quelle option TLS modifier au lieu d'afficher une erreur TLS générique.
- **ClickHouse 22.8 et antérieurs :** la connexion n'échoue plus avec `Unknown setting http_write_exception_in_output_format` ; DBine cesse de demander ce paramètre aux serveurs qui ne l'ont pas.
- **Notebooks, export Python :** une connexion PostgreSQL qui vérifie le certificat du serveur est exportée avec `sslmode=verify-full`, comme le fait **Copier la chaîne de connexion**, au lieu de `require`, qui ne le vérifie pas.
- **HashiCorp Vault, sécurité :** l'adresse Vault d'une source de mots de passe doit être en `https://` ; `http://` simple n'est accepté que pour un Vault sur cet ordinateur, comme pour les autres gestionnaires de secrets, de sorte que le jeton Vault ne circule jamais en clair sur le réseau.

## [1.1.0] - 2026-10-21

### Nouveautés
- **État de la synchronisation cloud en un coup d'œil :** la barre d'état affiche **Synchronisé** avec un point vert, **En attente** en orange tant qu'une modification attend d'être envoyée, et du rouge en cas d'erreur ; survolez pour voir l'heure de la dernière synchronisation, cliquez pour ouvrir les paramètres de synchronisation.
- **Copier la chaîne de connexion :** faites un clic droit sur une connexion (ou utilisez le bouton de son formulaire) et choisissez **Copier la chaîne de connexion…** pour l'obtenir sous forme de chaîne URI, ADO.NET, JDBC, SQLAlchemy, ODBC, libpq, Node.js ou Go, selon celles que gère son moteur, pour n'importe quelle base de données, avec ses paramètres TLS et de connexion. Le mot de passe est omis (un marqueur `${DB_PASSWORD}`) sauf si vous activez **Inclure le mot de passe** ; il est alors récupéré au moment de la copie et effacé du presse-papiers une minute plus tard s'il y est encore.
- **Étape Envoyer un message** dans les tâches planifiées : publiez le déroulement de l'exécution sur Microsoft Teams, Slack, Telegram ou ntfy, avec les variables de la tâche (`{results}` liste ce qu'a fait chaque étape), **Seulement si…** comme l'étape e-mail et un bouton **Tester**. L'URL du webhook et les jetons sont conservés dans le trousseau, jamais dans la tâche, et les messages ne passent que par HTTPS vers les serveurs du service lui-même.

### Corrections
- **Tunnels SSH, sécurité :** la bibliothèque SSH est mise à jour pour corriger deux avis (CVE-2026-111061, un déni de service via l'agent SSH, et CVE-2026-111062).
- **Clés RSA, sécurité :** la connexion SSH avec une clé RSA et l'ouverture des clés d'organisation de Zoho Vault et de Bitwarden s'exécutent désormais dans AWS-LC en temps constant, de sorte que la fuite par canal temporel de la bibliothèque RSA (RUSTSEC-2023-0071, « Marvin ») ne s'applique plus. Les clés RSA signent avec SHA-256 ou SHA-512 ; un serveur qui n'accepte que SHA-1 (`ssh-rsa`) demande une clé Ed25519 ou ECDSA, ou l'agent SSH.
- **TLS, sécurité :** la bibliothèque TLS et ses vérifications de certificats sont mises à jour dans DBine et dans 33 pilotes (RUSTSEC-2026-0346 à 0349 et 0381 à 0383), de sorte que les certificats d'autorités qui ne sont plus approuvées sont refusés. Les pilotes mis à jour se téléchargent tout seuls.
- **Moteur d'IA intégré :** lors de la décompression du moteur, seules les permissions de lecture, d'écriture et d'exécution des fichiers sont conservées.
- **Tâches planifiées, sécurité :** `{results}` et `{alerts}` ne remplissent plus que le texte des e-mails et des messages. Le SQL d'un script, le nom d'un fichier ou d'un dossier, une adresse ou le chemin d'une pièce jointe ne les prennent plus, de sorte qu'une erreur de base de données qui reprend des données forgées ne peut pas se transformer en SQL ni en chemin.
- **Messages des tâches planifiées :** `{results}` et `{alerts}` dans un message ou un e-mail apparaissent dans la langue de l'interface de DBine au lieu de toujours être en espagnol, y compris quand la tâche s'exécute DBine fermé.
- **Notebooks, gros résultats DataFrame SQL :** **Export avancé… › Toutes** écrit le résultat complet, pas seulement les lignes affichées. Le SQL de la cellule est de nouveau exécuté dans DuckDB sur les DataFrames lus lors de sa dernière exécution, sans relire de fichiers ni réinterroger les sources. Ses graphiques sont calculés d'emblée sur tout le résultat, et cela peut être désactivé. Lorsqu'un résultat est tronqué, la grille indique aussi que le tri et **Tout copier** ne portent que sur les lignes chargées.

## [1.0.0] - 2026-10-19

### Nouveautés
- **Thème clair :** un thème clair et une option **Comme le système** (par défaut) qui suit votre système d'exploitation quand il change ; zoom de l'interface avec Cmd/Ctrl + « + », « − », « 0 » ou la molette de la souris, et tailles de police distinctes pour l'éditeur et la grille de résultats, dans **Paramètres › Général**.
- **Mode Vim et vos propres raccourcis :** mode Vim pour l'éditeur SQL (Paramètres › Général), et chaque raccourci clavier peut être modifié dans **Paramètres › Raccourcis clavier**, avec recherche, réinitialisation et avertissements de conflit.
- **Éditeur scindé :** jusqu'à trois groupes d'éditeurs côte à côte ou l'un au-dessus de l'autre (le menu de l'onglet, le bouton de la barre d'onglets ou Cmd/Ctrl+\) ; faites glisser des onglets entre les groupes ou sur la moitié droite ou inférieure d'un groupe pour le scinder. La disposition est restaurée au redémarrage.
- **Connexion cloud pour les bases gérées :** les connexions PostgreSQL et MySQL vers Amazon RDS et Aurora, Google Cloud SQL et AlloyDB, et Azure Database peuvent se connecter avec **AWS IAM**, **Google Cloud IAM** ou **Microsoft Entra ID**. DBine génère un jeton neuf à partir de votre profil AWS (SSO inclus), de gcloud ou de az avant chaque connexion : fini les jetons à coller qui expiraient au bout de 15 minutes, et **Tester** indique avec quelle identité la connexion a été faite.
- **Mots de passe depuis une commande ou un gestionnaire de secrets :** n'importe quel champ secret de n'importe quel moteur, ainsi que le mot de passe et la phrase secrète de la clé du tunnel SSH, peuvent prendre leur valeur d'une commande, de 1Password, HashiCorp Vault, AWS Secrets Manager, Azure Key Vault ou Google Secret Manager. Elle est récupérée à chaque connexion et conservée en mémoire uniquement, avec une option de clé JSON et un test par champ qui n'affiche que la longueur. Une commande ou un gestionnaire de secrets issu d'un import ou d'une sauvegarde restaurée ne s'exécute qu'après votre confirmation sur cet ordinateur.
- **KeePass et Zoho Vault comme sources de mots de passe :** n'importe quel champ secret (et celui du tunnel SSH) peut prendre sa valeur d'une entrée d'une base KeePass (.kdbx, lue par DBine lui-même ; mot de passe principal issu du trousseau, demandé à la connexion ou obtenu par une commande ; fichier de clé facultatif) ou d'un secret Zoho Vault (n'importe quel centre de données, déchiffré sur votre ordinateur avec votre phrase secrète principale), et, si vous le souhaitez, l'utilisateur de la connexion provient de la même entrée.
- **Identifiants de base de données de Vault, CyberArk et Delinea comme sources de mots de passe :** les mots de passe des connexions peuvent provenir du moteur de secrets de bases de données de HashiCorp Vault (identifiants créés pour la connexion, renouvelés tant qu'elle est ouverte et révoqués à sa fermeture ; connexion AppRole également pour Vault), de CyberArk Conjur, du Central Credential Provider de CyberArk (avec certificats client) et de Delinea Secret Server.
- **Bitwarden, Bitwarden Secrets Manager et AWS Parameter Store comme sources de mots de passe :** n'importe quel champ secret (et celui du tunnel SSH) peut prendre sa valeur d'un élément Bitwarden, personnel ou d'une organisation, sur bitwarden.com, bitwarden.eu ou votre propre serveur, déchiffré sur votre ordinateur avec votre mot de passe principal ; d'un secret Bitwarden Secrets Manager (avec le jeton d'accès d'un compte machine) ; ou d'un paramètre AWS Systems Manager (SecureString, String ou un élément d'une StringList), avec une clé JSON et, si vous le souhaitez, l'utilisateur de la connexion issu du même élément.
- **Les identifiants d'abord, et parcourez votre gestionnaire de secrets :** le formulaire de connexion demande d'abord les **Identifiants** : conserver le mot de passe dans le trousseau, le demander à chaque fois, ou prendre l'utilisateur et le mot de passe d'une ligne de commande ou d'un gestionnaire de secrets, dont les champs les remplacent. Un bouton **Parcourir** liste les entrées d'un gestionnaire et les noms de leurs champs pour que vous les choisissiez au lieu de saisir les références ; il n'affiche que des noms, jamais des valeurs.
- **Utilisateur et mot de passe depuis l'entrée :** avec KeePass, Bitwarden, 1Password, Zoho Vault, Delinea ou CyberArk CCP comme identifiants, l'utilisateur comme le mot de passe proviennent de l'entrée, et **Paramètres avancés** permet de choisir d'autres champs dans la liste de l'entrée elle-même. Chaque source demande d'abord sa connexion et l'entrée en dernier, et Parcourir est prêt une fois cela renseigné.
- **Comptes Zoho Vault, connectés une seule fois :** un assistant guidé **Connecter Zoho Vault** vous accompagne dans la console de Zoho, trouve votre centre de données et vérifie votre phrase secrète principale ; ensuite, n'importe quelle connexion choisit simplement le compte. Les comptes restent sur cet ordinateur et se gèrent dans **Paramètres › Gestionnaires de secrets**.
- **Des comptes pour chaque gestionnaire de secrets :** Bitwarden, Bitwarden Secrets Manager, HashiCorp Vault et OpenBao, Delinea, CyberArk Conjur et Central Credential Provider, 1Password, AWS, Azure Key Vault, Google Secret Manager et KeePass se connectent désormais une seule fois dans **Paramètres › Gestionnaires de secrets** avec un assistant guidé qui explique où se trouve chaque valeur, vérifie le compte en direct et indique qui s'est connecté ; ensuite, n'importe quelle connexion choisit simplement le compte, et les identifiants ne vont qu'au serveur propre de ce gestionnaire.
- **Tenants Azure à choisir :** l'assistant de comptes Azure Key Vault liste vos tenants Microsoft Entra (vous pouvez toujours en saisir un), et quand vous n'êtes pas connecté au tenant choisi, il affiche la commande `az login --tenant` à copier.
- **Saisie semi-automatique dans les cellules DataFrame SQL :** les cellules du dessus sont suggérées comme des tables (même celles qui n'ont pas encore été exécutées), avec leurs colonnes après `nom.` ou un alias et dans la liste du select, et les lecteurs de fichiers de DuckDB après FROM et JOIN ; choisir un lecteur place le curseur entre ses guillemets, où **Choisir un fichier…** ouvre la boîte de dialogue de fichiers. La liste des tables après FROM ne se ferme plus toute seule pendant l'actualisation du notebook.
- **Vue carte :** les résultats comportant des colonnes de géométrie (PostGIS, MySQL, SQL Server, SQLite/SpatiaLite, DuckDB, MongoDB, Elasticsearch et autres) ou des colonnes de latitude/longitude peuvent être affichés sur une carte, dessinée sur votre machine par-dessus un planisphère intégré hors ligne. Les tuiles de carte en ligne plus détaillées peuvent être activées dans **Paramètres › Cartes** ; le fournisseur voit alors quelle zone vous regardez, jamais les données.
- **Oracle sur la carte :** les colonnes SDO_GEOMETRY s'affichent désormais sur la carte des résultats, aussi bien dans les données d'une table que dans vos propres requêtes (converties sur le serveur en WKT avec leur SRID) ; ces cellules sont en lecture seule dans la grille.
- **Ouvrir directement des fichiers de données :** les fichiers SQLite, DuckDB, CSV, TSV, Parquet, JSON et Excel s'ouvrent par double-clic ou avec « Ouvrir avec DBine », en les déposant sur la fenêtre, ou avec **Fichier › Ouvrir un fichier…** (Cmd/Ctrl+O). Chacun s'ouvre comme une connexion temporaire que vous pouvez conserver avec **Enregistrer la connexion**, et les fichiers de données affichent aussitôt leurs 1 000 premières lignes.
- **Palette de commandes :** Cmd/Ctrl+Shift+P trouve et exécute n'importe quelle action, en n'affichant que ce que prend en charge le moteur actuel, et **Aller à…** (Cmd/Ctrl+P) saute vers les connexions, bases, tables, vues, routines, requêtes enregistrées et onglets ouverts. Chaque action de la palette peut aussi recevoir son propre raccourci.
- **Sauter vers les lignes liées :** dans les données d'une table ou les résultats d'une requête, **Aller à la ligne référencée** (ou Cmd/Ctrl+clic) ouvre la table parente filtrée par cette clé étrangère, et **Lignes qui la référencent** ouvre chaque table enfant filtrée par la valeur, clés composites comprises. Sur tous les moteurs avec clés étrangères.
- **Notebooks SQL :** **Fichier › Nouveau notebook** ouvre un notebook : des cellules SQL qui s'exécutent sur n'importe quelle connexion, chacune conservant son résultat et son graphique en dessous, et des cellules de texte pour les expliquer. Il est enregistré comme un `.ipynb` Jupyter standard que GitHub, VS Code et Jupyter affichent, et que peuvent aussi ouvrir Azure Data Studio et les notebooks `%%sql` de Jupyter. Le fichier conserve un aperçu de chaque résultat (100 lignes et 1 Mo par cellule, dans **Paramètres › Notebooks**) avec son nombre total de lignes, l'heure et la durée, jamais les détails de connexion ni les mots de passe ; dans un projet, les notebooks sont enregistrés sans résultats sauf si vous l'activez. L'assistant peut expliquer un résultat, proposer la cellule suivante ou une version groupée d'une grosse requête, sans voir les lignes. Export vers HTML ou Markdown.
- **Graphiques sur tout le résultat :** les graphiques sont calculés sur toutes les lignes du résultat, pas seulement celles chargées dans la grille. Sur les moteurs SQL, le regroupement se fait sur le serveur, dans les cellules DataFrame SQL d'un notebook c'est DuckDB qui s'en charge, et sur les autres moteurs c'est DBine.
- **Plus de types de graphiques :** histogramme, carte de chaleur, boîte à moustaches (avec moustaches et valeurs aberrantes), barres avec lignes et KPI (le total de tout le résultat).
- **Plusieurs graphiques par cellule de notebook :** une cellule de notebook peut afficher plus d'un graphique de son résultat, et mémorise quelle source utilise sa carte.
- **Graphiques dans les exports de notebooks :** les notebooks exportés en HTML ou en Markdown incluent leurs graphiques sous forme d'images.
- **Croisez des résultats de bases différentes dans un notebook :** le résultat de chaque cellule est un DataFrame portant le nom de la cellule (affiché et modifiable dans son en-tête), et une cellule **DataFrame SQL** interroge et joint les DataFrames du dessus comme des tables avec le SQL de DuckDB et sans connexion, de sorte qu'une cellule peut joindre un résultat PostgreSQL avec un résultat SQL Server et un résultat MongoDB. Elle s'exécute dans une base DuckDB en mémoire qui ne peut ni écrire de fichiers ni accéder au réseau, et qui est supprimée à la fermeture du notebook ; les lignes de chaque source proviennent de son exécution dans cette session ou de sa requête réexécutée, en lecture seule, sur sa propre connexion (jusqu'à 1 000 000 de lignes par DataFrame, dans **Paramètres › Notebooks**).
- **Notebooks comme rapports planifiés :** une tâche planifiée peut exécuter un notebook DBine fermé (**Exécuter un notebook**), enregistrer la copie exécutée et l'exporter en HTML ou en Markdown, et l'étape suivante **Envoyer un e-mail** peut la joindre ou envoyer l'export HTML comme corps de l'e-mail. Rien n'est demandé sans surveillance : elle ne lit que les fichiers avec **Toujours autoriser** pour ce notebook, s'exécute sur les environnements qui demandent confirmation seulement avec l'autorisation explicite de l'étape, et ses cellules qui modifient des données s'ajoutent à l'approbation de la tâche. Les exports planifiés incluent les tableaux ; les graphiques sont inclus lors d'un export depuis l'application.
- **Fichiers locaux dans les cellules DataFrame SQL :** une cellule DataFrame SQL peut lire des fichiers CSV, Parquet, JSON ou NDJSON et Excel de votre ordinateur avec `read_csv`, `read_parquet`, `read_json` et `read_xlsx`, y compris des motifs comme `data/*.csv` et des listes de fichiers, des fichiers géographiques (GeoJSON, Shapefile avec ses fichiers associés, GeoPackage, KML, GPX, FlatGeobuf) avec `st_read`, et des tables de fichiers de base SQLite et DuckDB avec `sqlite_scan` et `read_duckdb`, chacun sous forme de DataFrame qu'on peut joindre aux résultats de cellules de n'importe quel moteur ; les géométries continuent de fonctionner avec les fonctions spatiales comme `ST_Contains` et s'affichent sur la carte. Les fichiers sont seulement lus, jamais écrits. Avant de lire un fichier pour la première fois, le notebook demande : l'autoriser pour cette exécution, toujours pour ce notebook sur cet ordinateur (un motif couvre ses nouveaux fichiers), ou le refuser ; les identifiants, les clés, l'historique du shell et les fichiers système ne sont jamais lus. Si le contenu d'un fichier a changé depuis la dernière exécution, le résultat l'indique. Paramètres › Notebooks liste les fichiers que chaque notebook peut lire et permet de les retirer. Les mêmes lecteurs lisent aussi des fichiers du web (`https://`) et d'Amazon S3, Google Cloud Storage et Azure Blob Storage (`s3://`, `gs://`, `az://`, `abfss://`, avec des motifs comme `s3://bucket/2026/*.parquet`), avec la même approbation, qui affiche d'abord le serveur ; DBine les télécharge lui-même dans un dossier temporaire privé supprimé après la lecture, en utilisant la connexion AWS, Google Cloud ou Azure de cet ordinateur (un profil AWS peut être choisi dans Paramètres › Notebooks), et n'atteint jamais d'adresses locales ou de réseaux privés, ne suit pas les redirections vers d'autres serveurs et n'accepte pas d'identifiants dans l'URL. Un GeoPackage ouvert dans un autre programme inclut désormais les modifications encore présentes dans son `-wal`.
- **Exporter un notebook en notebook Python :** **Exporter › Notebook Python (.ipynb)** écrit un notebook Jupyter qui le reproduit : un moteur SQLAlchemy (ou le client Python propre au moteur, comme pymongo ou redis) par connexion, chaque cellule de requête lue dans un DataFrame pandas portant le nom de la cellule, les cellules DataFrame SQL exécutées avec DuckDB sur ces DataFrames, et les cellules de texte en Markdown. Les mots de passe et les jetons ne sont jamais exportés : le code les lit dans des variables d'environnement, que la première cellule liste avec les paquets à installer. Les résultats sont omis sauf si vous choisissez d'inclure les aperçus.
- **Écrire un DataFrame dans une table depuis un notebook :** une **cellule d'écriture** (ajoutée sous n'importe quelle cellule avec **Écrire dans une table…**) écrit le résultat entier d'une cellule dans une table de SQL Server, PostgreSQL, MySQL, MariaDB, Oracle, SQLite, DuckDB, MongoDB et autres : insérer, mettre à jour par clé, insérer ou mettre à jour, ou remplacer le contenu de la table, avec un mappage des colonnes, un aperçu et une nouvelle table si vous le souhaitez. Chaque écriture demande d'abord confirmation, en indiquant où, comment et combien de lignes (les connexions de production demandent de saisir le nom de la table), s'exécute dans une seule transaction sur les moteurs qui en ont, et les connexions en lecture seule ne sont jamais écrites. Dans une tâche planifiée, une cellule d'écriture ne s'exécute que lorsque l'étape autorise cette cellule, cette table et ce mode.
- **Paramètres de notebook :** un notebook peut définir des paramètres (texte, nombre, date, date et heure, booléen ou liste fixe, avec une valeur par défaut) que ses cellules utilisent sous la forme `{{name}}` ; à l'exécution, les valeurs manquantes sont demandées, chaque valeur arrive à la base comme une donnée (liée par le pilote ou entre guillemets selon les règles propres du moteur), et chaque résultat affiche les valeurs qui l'ont produit.
- **Cinq langues de plus :** DBine et son application pour téléphone parlent désormais allemand, russe, chinois simplifié, japonais et coréen, en plus de l'espagnol, de l'anglais, du portugais, du français et de l'italien ; la langue suit celle de votre système et seule celle utilisée est chargée.

### Améliorations
- **Signée sur macOS :** l'application macOS est signée avec un Apple Developer ID et notarisée par Apple, elle s'ouvre donc sans l'avertissement « impossible de vérifier le développeur » ni aucune commande de terminal. La première fois que la version signée s'ouvre, macOS demande une seule fois si DBine peut utiliser sa clé dans le trousseau : choisissez **Toujours autoriser**.
- **La connexion cloud ne fait confiance qu'au certificat de l'instance :** les connexions qui se connectent avec AWS IAM, Google Cloud IAM ou Microsoft Entra, et les connexions Cloud SQL et AlloyDB avec le fichier CA de l'instance, n'acceptent désormais que cette CA (ou le bundle RDS d'Amazon), et non n'importe quel certificat public, avant d'envoyer un jeton ou un mot de passe. Un serveur qui demande le mot de passe en clair alors que **Faire confiance au certificat du serveur** est activé échoue désormais au lieu de le recevoir.
- **Signée sur Windows :** l'installeur Windows et l'application sont signés avec un certificat de signature de code, donc Windows affiche qui publie DBine au lieu de « Éditeur inconnu ».

### Corrections
- **Monitor du serveur :** ses chiffres, tableaux et notes sont traduits dans tous les moteurs, au lieu d'afficher certains libellés en espagnol.
- **Assistant IA et optimiseur :** les instructions au modèle sont dans la langue de l'interface, donc un modèle local répond dans cette langue au lieu de l'espagnol. La ligne qui indique ce qui a été envoyé au modèle est elle aussi traduite.

## [0.1.14] - 2026-10-17

### Nouveautés
- **DBine sur votre téléphone (Android) :** dans **Paramètres › Appareils**, activez **Autoriser les téléphones** et associez l'application DBine du téléphone en scannant un code QR, puis confirmez sur l'ordinateur que les deux écrans affichent le même code. Depuis le téléphone, vous voyez le Monitor de chaque connexion, qui bloque qui, les sessions en cours et le bilan de santé d'une base de données ; dans **Activité**, les requêtes que vous exécutez dans l'éditeur de DBine et les tâches planifiées, avec leur détail. Si vous autorisez ce téléphone, terminer une session, annuler une requête, ou exécuter ou annuler une tâche se fait chaque fois avec votre empreinte, votre visage ou votre code PIN. Le téléphone vous avertit quand une requête longue se termine et quand une tâche échoue ou comporte une alerte, même application fermée (toutes les 15 minutes). Vos connexions et vos mots de passe ne quittent jamais l'ordinateur : le téléphone interroge DBine par un canal chiffré de bout en bout.
- **Accès du téléphone depuis l'extérieur de votre réseau :** désactivé par défaut (réseau local uniquement). Activez-le dans **Paramètres › Appareils › Hors du réseau local**, via votre propre serveur de relais ou les serveurs publics d'iroh, qui voient les adresses IP mais jamais le contenu. Comment il est protégé : [dbine.com/fr/phone-security](https://dbine.com/fr/phone-security).
- **Les requêtes longues gardent l'ordinateur éveillé :** tant qu'une requête de l'éditeur s'exécute depuis 30 secondes ou plus, DBine empêche l'ordinateur de se mettre en veille, afin que la connexion ne soit pas coupée.
- **Téléchargement de l'application du téléphone :** l'application du téléphone (Android) est publiée à chaque version et sur dbine.com.

## [0.1.13] - 2026-10-16

### Améliorations
- **Soutenir le projet :** le bouton en forme de cœur (**Soutenir le projet**), son rappel et **Aide › Soutenir le projet** ouvrent Buy Me a Coffee.
- **Script avec les données de tables choisies :** dans **Générer le script de la base**, lorsque **Données (INSERT)** est coché, vous pouvez garder les données de toutes les tables ou choisir lesquelles en reçoivent, depuis une liste avec filtre (**Choisir des tables…**), de sorte que le script puisse contenir la structure de toute la base et les lignes de quelques tables seulement.
- **Menu de la base :** **Exporter la base…** a été retiré. Il ouvrait la même boîte de dialogue que **Générer un script…**, qui reste et fait tout ce qu'il faisait (structure, données et enregistrement dans un fichier).
- **Statistiques d'utilisation :** lorsque le serveur de statistiques ne répond pas ou échoue, DBine réessaie pendant une heure au plus au lieu d'abandonner l'événement. Rien d'autre ne change dans ce qui est envoyé, et désactiver les statistiques arrête tout nouvel essai en attente.
- **Téléchargement Flatpak :** chaque version inclut désormais un fichier `.flatpak` pour Linux (`x86_64` et `aarch64`), à installer avec `flatpak install --user ./dbine-X.Y.Z-x86_64.flatpak` ; Flatpak récupère lui-même le runtime GNOME depuis Flathub. Il ne se met pas à jour tout seul : installez le fichier de la version suivante de la même manière.

## [0.1.12] - 2026-10-15

### Nouveautés
- **Flathub :** DBine est soumis à Flathub sous le nom `io.addlayer.dbine`. Cette version inclut tous les drivers, DuckDB et le moteur de l'assistant IA, donc rien n'est téléchargé à la première utilisation, et c'est Flatpak, et non DBine, qui la tient à jour. Dans le sandbox, l'action du pare-feu Azure explique qu'elle ne peut pas utiliser Azure CLI et propose le portail, T-SQL ou un terminal, et les tâches planifiées sont enregistrées mais ne peuvent pas être inscrites dans le planificateur du système.
- **winget :** DBine est soumis à winget sous Windows sous le nom `AddLayer.DBine` (`winget install AddLayer.DBine` une fois accepté), et chaque nouvelle version y arrive d'elle-même. Installé avec winget, DBine continue de se mettre à jour tout seul comme avant.

### Améliorations
- **Nouvel identifiant de l'application :** DBine devient `io.addlayer.dbine`. Au premier démarrage après la mise à jour, vos connexions, mots de passe enregistrés, onglets ouverts, conversations de l'assistant, réglages et tâches planifiées sont copiés automatiquement ; les données de la version précédente restent telles quelles, donc y revenir ne fait rien perdre. Sous Windows et Linux, un gros modèle d'IA peut être téléchargé de nouveau. Sous macOS, le trousseau peut demander l'accès une fois, et certaines autorisations de confidentialité (comme l'accès au réseau local) peuvent être redemandées ; sous Windows, une épingle de la barre des tâches créée avec une version antérieure peut devoir être épinglée de nouveau.

## [0.1.11] - 2026-10-13

### Nouveautés
- **Trier par colonne :** cliquez sur l'en-tête d'une colonne dans les données d'une table pour trier selon elle (une deuxième fois pour l'ordre décroissant, une troisième pour l'effacer ; Shift+clic ajoute d'autres colonnes). La base trie toute la table lorsque le moteur le permet, comme pour les filtres ; sinon, ou lorsque le serveur refuse l'ordre, les lignes chargées sont triées et un avis en explique la raison. Les résultats d'une requête peuvent être triés de la même façon.
- **Contrôles de la barre d'état :** dans un onglet de requête, la barre d'état bleue contient désormais la connexion, la base de données, le mode de transaction (Auto par défaut, ou Manuel) et la limite de lignes. Cliquez sur l'un d'eux pour le modifier avec un sélecteur rapide, comme dans VS Code. La barre d'outils de la requête ne perd plus de boutons lorsque le panneau latéral est large : ce qui ne tient pas passe dans un menu **…**.
- **Envoyer un commentaire :** un bouton en bas de la barre d'activité ouvre un court formulaire pour signaler un bogue, suggérer une amélioration, demander un nouveau moteur ou poser une question. GitHub s'ouvre avec le ticket déjà rempli et étiqueté, il ne reste qu'à appuyer sur « Create ». Le diagnostic facultatif (version de DBine, système d'exploitation, langue, ainsi que le moteur et la version du serveur de la connexion active) est affiché avant d'être inclus, et ne contient jamais de serveurs, d'utilisateurs, de noms de bases ni de requêtes.

### Améliorations
- **Texte long dans la grille :** en s'arrêtant sur une cellule dont le texte ne tient pas, on le voit en entier dans une infobulle qui conserve les sauts de ligne et que l'on peut faire défiler et sélectionner.
- **Quand une connexion échoue :** développer une connexion commence par la connecter et affiche **Connexion…** ; l'arborescence n'apparaît que lorsque le serveur répond (alors instantanément, à partir de ce que DBine mémorise, pendant son actualisation). Si la connexion est impossible, elle affiche un bandeau avec **Réessayer** et **Voir ce qui se passe…**, et les onglets (les données d'une table, l'exécution d'une requête et les autres outils) affichent la même erreur au lieu de rester vides. Si la connexion tombe plus tard, elle revient au bandeau. **Voir ce qui se passe…** explique l'échec (pare-feu, délai dépassé, refus, nom d'hôte, certificat ou connexion) et ce qu'il faut vérifier, et sur Azure SQL il peut ajouter votre IP au pare-feu du serveur avec votre session Azure CLI, après confirmation du serveur, de la règle et de l'IP. Fonctionne sur tous les moteurs ; la cause est plus précise sur SQL Server, Azure SQL, PostgreSQL et MySQL.

### Corrections
- **Éditeur de requêtes :** un onglet dont la connexion n'est pas encore ouverte (par exemple, restauré au démarrage) complète de nouveau les noms de schémas, de tables et de colonnes : demander la complétion ouvre la connexion et la liste apparaît quand les noms arrivent. Les snippets ne s'ajoutent plus à la liste après un point.
- **Éditeur de requêtes :** la liste de complétion et les aides s'ouvrent vers le haut quand vous tapez sur les dernières lignes, au lieu d'être masquées par les résultats.
- **Pare-feu d'Azure SQL :** **Ajouter mon IP au pare-feu Azure** n'utilise que l'IP publique de cette machine, obtenue en HTTPS et vérifiée à nouveau avant la création de la règle. Une IP citée dans le message d'erreur d'un serveur est affichée comme non vérifiée et n'est jamais utilisée.
- **Réessayer après un échec de connexion :** si, entre-temps, l'onglet est passé à une autre connexion ou base de données, réessayer redemande la confirmation de production.
- **Renommage sur Snowflake et sur Trino, Presto et Starburst :** ce qui désigne l'objet renommé (vues, vues matérialisées, fonctions, procédures et, sur Snowflake, tâches, streams et tables dynamiques) est listé pour que vous le corrigiez à la main au lieu d'être recréé, afin de conserver son propriétaire et ses paramètres de sécurité. Le renommage est aussi refusé lorsque le code stocké serait scindé en plusieurs instructions, et la réécriture des dépendants ne change plus l'identité sous laquelle le code s'exécute (`DEFINER`, `SQL SECURITY`, `AUTHID`, `EXECUTE AS`).
- **Requêtes des clients IA et de l'assistant :** une requête s'exécute sans demander d'approbation uniquement lorsque la base l'impose en lecture seule et qu'elle n'appelle que des fonctions natives du moteur, sans effets de bord. Une requête qui appelle toute autre fonction (une fonction utilisateur, ou une qui agit hors de la transaction, comme suspendre la réplication ou prendre un verrou) attend d'abord votre approbation.

## [0.1.10] - 2026-10-10

### Nouveautés
- **Les pilotes se mettent à jour tout seuls, séparément de l'application :** DBine cherche dans un index signé le pilote le plus récent compatible avec votre version, le télécharge en arrière-plan et revient au précédent en cas d'échec. Dans Paramètres › Pilotes, il y a un bouton **Rechercher des mises à jour**, l'état de chaque pilote et **Revenir à la précédente**. Un pilote peut être publié seul, sans nouvelle version de l'application.
- **Renommer une base de données :** **Renommer…** sur une base dans l'explorateur. La boîte de dialogue affiche les autres sessions ouvertes dessus (que le renommage termine), si le nouveau nom existe déjà, combien d'objets sont déplacés et le script complet. Ensuite, la base par défaut de la connexion, les onglets ouverts, les requêtes enregistrées, les migrations, les cibles de projets et les étapes de tâches planifiées suivent le nouveau nom ; les tâches qui modifient des données dedans demandent à nouveau une approbation. Disponible dans SQL Server, Azure SQL, Babelfish, la famille PostgreSQL, MySQL, MariaDB, Snowflake et MongoDB ; lorsque le moteur ne peut pas renommer (ou déplacer) une base, l'option n'est pas proposée.
- **À quelle colonne correspond cette valeur ?** Dans un `INSERT … VALUES`, placer le curseur sur une valeur affiche une infobulle avec sa colonne (par exemple « Colonne 14 sur 48 : Name ») et met cette colonne en évidence dans la liste. Sans liste de colonnes, elle utilise celles de la table dans l'ordre. Fonctionne sur tous les moteurs SQL et CQL.
- **Plus de façons de se connecter à SQL Server avec Microsoft Entra ID :** interactive avec MFA (dans le navigateur), intégrée (le compte Windows, via l'ADFS fédéré de l'organisation), identité managée et par défaut (variables d'environnement, identité managée, Azure CLI ou Azure Developer CLI). Le jeton est conservé tant que DBine est ouvert et renouvelé avant son expiration.
- **Associer une connexion à une base :** **Associer une connexion…** dans Utilisateurs et droits crée l'utilisateur de la base pour une connexion du serveur qui existe déjà (`CREATE USER … FOR LOGIN`, avec son schéma par défaut), en listant les connexions qui n'ont pas encore d'utilisateur à cet endroit. Disponible sur SQL Server, Azure SQL Database (la connexion y est saisie), Babelfish et SAP ASE.

### Améliorations
- **Ce qu'apporte chaque version :** l'avis de nouvelle version affiche ses changements et ceux des versions intermédiaires, à partir de celle que vous avez installée, dans la langue de l'application.
- **Anciennes versions de l'application :** désormais, une application plus ancienne que les cinq dernières versions doit être mise à jour pour télécharger de nouveaux pilotes. Les pilotes qu'elle a déjà installés continuent de fonctionner.
- **Utilisateurs et droits :** les modifications sont mises en file et appliquées ensemble. Une barre **Modifications en attente** les liste, une seule revue montre tout le script, et elles s'exécutent une par une : celles qui sont appliquées quittent la liste et, si l'une échoue, elle et les suivantes restent en attente.
- **Les lectures des clients MCP et de l'assistant IA sont protégées par la base de données elle-même :**
  - **Garanti par le serveur :** sur la famille PostgreSQL, MySQL, MariaDB, SQLite et libSQL, chaque lecture s'exécute comme une seule instruction dans une transaction en lecture seule que le serveur impose et que DBine annule toujours.
  - **Approbation :** sur les autres moteurs, chaque lecture demande votre approbation dans DBine, comme les écritures. La boîte de dialogue d'approbation indique s'il s'agit d'une lecture ou d'une écriture, et **Tout approuver** s'applique séparément aux lectures et aux écritures.
  - **Contrôle des écritures :** DBine vérifie toujours chaque requête à la recherche d'écritures, comme couche supplémentaire.

### Corrections
- **Le mode lecture seule est plus strict :** les requêtes des clients MCP au niveau Lecture, de l'assistant IA et des connexions en lecture seule sont vérifiées mot par mot, et pas seulement par leur premier mot. Elles refusent désormais :
  - une écriture cachée après une lecture dans un lot SQL Server ;
  - un `WITH` qui modifie des données ;
  - `SELECT … INTO`, `EXEC` et `SET` ;
  - les fonctions qui agissent en dehors de la requête, comme `set_config`, `dblink_exec` et `xp_cmdshell`.

  Un plan estimé refuse les scripts qui désactiveraient le mode plan, si bien que les alternatives IA de **Optimiser la requête** ne peuvent rien exécuter. Les tâches planifiées dont les scripts comptent désormais comme de l'écriture demandent à nouveau une approbation.
- **Bibliothèque avec git :** un dépôt partagé ne peut plus faire lire, écrire ou supprimer à DBine des fichiers en dehors du dossier de la Bibliothèque.
- **Tunnels SSH :** la clé d'hôte de chaque serveur est vérifiée séparément. Une clé acceptée pour un hôte de rebond ne vaut plus pour le serveur suivant, `known_hosts` est consulté en premier et une clé modifiée est toujours refusée. Les serveurs que vous aviez acceptés auparavant sont demandés une fois de plus. Le port local du tunnel ne sert que les programmes de votre propre utilisateur.
- **Sauvegarde dans le cloud :** un fichier de sauvegarde modifié par quelqu'un d'autre ne peut plus affaiblir le chiffrement de votre prochain envoi.
- **libSQL / Turso :** un `authToken` dans une URL collée est conservé dans le trousseau du système, et non dans l'adresse, le nom ou l'historique de la connexion. Les connexions enregistrées auparavant sont nettoyées au démarrage de DBine.
- **Documenter la base :** les noms de colonnes ne peuvent pas injecter de HTML dans le dictionnaire de données Markdown.
- **Notifications de tâches sous Windows :** un message d'erreur de la base ne peut plus exécuter de commandes via la notification. Sous macOS et Linux, les notifications reçoivent aussi le texte sous forme d'arguments séparés.
- **Tunnels SSH :** un serveur dont vous avez accepté la clé dans DBine et qui en présente maintenant une autre est refusé, au lieu de vous le redemander. La section SSH de la connexion liste les serveurs acceptés, chacun avec **Oublier**.
- **SQL contre PostgreSQL :** les valeurs de type chaîne sont écrites sous la forme `E'…'` avec les barres obliques inverses échappées, de sorte qu'une valeur ne peut pas fermer la chaîne trop tôt sur un serveur où `standard_conforming_strings` est désactivé. Cela couvre la famille PostgreSQL, CockroachDB et les moteurs similaires.
- **Scripts Snowflake :** une barre oblique inverse dans un `"nom entre guillemets"` ne change plus l'endroit où une instruction se termine.
- **Export CSV et TSV :** les cellules de texte et les noms de colonnes qui commencent par `=`, `+`, `-`, `@`, une tabulation ou un retour chariot reçoivent un `'` devant, pour que les tableurs ne les exécutent pas comme des formules. Les nombres ne sont jamais modifiés. Une option de la boîte de dialogue d'export la désactive.
- **Abandon des modifications dans Projets :** un fichier nommé comme un motif (`*`) n'abandonne que ce fichier.
- **Copier un sous-ensemble :** le masquage utilise une nouvelle clé aléatoire de 256 bits à chaque exécution.
- **Mises à jour des pilotes :** l'application n'accepte jamais un index de pilotes plus ancien que celui avec lequel elle a été publiée, même sur une nouvelle installation, ni un index qui n'est plus renouvelé. Les pilotes installés continuent de fonctionner dans les deux cas.
- L'import de connexions, le linter et le bilan de santé ne s'arrêtent plus sur les caractères accentués ou autres caractères multi-octets.
- **Lecture seule sur SQL Server :** chaque requête s'exécute dans une transaction toujours annulée. Les sauvegardes, les restaurations, l'activation ou la désactivation de triggers, les écritures par pointeur de texte, Service Broker et les instructions de transaction sont refusés lorsqu'ils viennent après une lecture dans le même lot.
- **Lecture seule sur PostgreSQL :** les noms écrits avec des échappements Unicode (`U&"…"`) sont refusés, afin qu'une fonction interdite ne puisse pas être appelée sous une autre graphie.
- **Export CSV et TSV :** la protection contre les formules s'applique aussi au texte stocké dans des colonnes déclarées numériques, ce que SQLite autorise.
- **Scripts générés :** les noms d'objets provenant du serveur ne peuvent plus terminer un commentaire et s'exécuter comme du code. Cela couvre les correctifs proposés par le **Contrôle de santé** ainsi que les scripts d'utilisateurs, de sauvegardes et de structure. Les noms ClickHouse contenant une barre oblique inverse sont correctement entre guillemets.
- **Lecture seule :** une instruction qui ne commence pas par un mot, comme un nom entre crochets qui exécute une procédure sur SQL Server, est refusée. Les mots à l'intérieur des lectures entre parenthèses sont eux aussi vérifiés.
- **Modifier la table :** les avertissements du script que vous ouvrez comme requête restent sur leur ligne de commentaire.
- **Les mots de passe et les options secrètes** sont masqués partout où ils sont modifiés, y compris dans les étapes de sauvegarde des tâches planifiées.
- **Voir les dépendances et Renommer** ne s'arrêtent plus sur les routines dont les noms entre guillemets sont inhabituels.
- **Lecture seule sur SQL Server :** un lot ne peut commencer que par `SELECT`, `WITH`, `USE` ou `PRINT`. Ce qui suit `SHOW`, `DESCRIBE` ou `PRAGMA` est vérifié sur tous les moteurs.
- **Importer des connexions :** une URL JDBC Oracle avec utilisateur et mot de passe les garde hors du nom de la connexion : le mot de passe va dans le trousseau du système.
- **Lecture seule sur SQL Server :** une procédure dont le nom commence comme `print_` ou `select1` n'est plus prise pour une lecture.
- **Importer des connexions :** un mot de passe Oracle contenant `@` est conservé en entier dans le trousseau du système.
- **Tunnels SSH sur Linux :** le port local du tunnel ne fait confiance qu'aux connexions ouvertes par votre propre utilisateur.
- **Lecture seule :** une requête contenant un retour chariot isolé ou une espace Unicode inhabituel est refusée, car les moteurs ne s'accordent pas sur l'endroit où un commentaire ou une instruction se termine dans ce cas. Les fonctions à effets de bord sont aussi refusées : `load_extension` dans SQLite, `pg_notify` et les lectures de réplication logique dans PostgreSQL, et l'annulation de sessions ou de requêtes dans Snowflake.
- **Importer des connexions :** une URL SQL Server sans hôte ne place plus son mot de passe dans le nom de la connexion.
- **Utilisateurs et droits :** l'aperçu masque les mots de passe PostgreSQL sous toutes leurs formes.
- **Lecture seule :** les requêtes qui prennent des verrous que d’autres sessions attendent sont refusées, par exemple `pg_advisory_lock`, `GET_LOCK`, `LOCK IN SHARE MODE` et `FOR SHARE`.
- **Snowflake et Cassandra :** les commentaires `//` sont lus comme le serveur les lit.
- **Les connexions SQL Server en lecture seule** refusent les hints de verrouillage et `WAITFOR`.
- **Les connexions en lecture seule** refusent d'avancer une séquence (`NEXT VALUE FOR`, `.NEXTVAL`).
- **Renommer :**
  - Sur tous les moteurs, un dépendant dont le code serait scindé en plus d'une instruction est refusé.
  - Les corps de routines Snowflake contenant `$$` sont conservés entiers.
  - Les noms OrientDB que DBine ne pourrait pas mettre entre guillemets en toute sécurité sont refusés.
- **Export SQL :** les valeurs de type chaîne sont écrites comme les lit le moteur source, de sorte qu'elles reviennent exactes quand vous exécutez le script dessus. Elles ne peuvent pas non plus ajouter d'instructions sur un moteur qui lit les barres obliques inverses autrement.
  - **PostgreSQL, SQL Server, Oracle, SQLite, DuckDB, Firebird, SAP HANA, Trino, Db2, Teradata, Vertica, Exasol, Netezza et Dremio :** la barre oblique inverse est écrite avec la fonction de caractères du moteur lui-même, si bien qu'aucune n'apparaît dans une chaîne. Les scripts Firebird contiennent une ligne par `INSERT`, le seul format que Firebird accepte.
  - **Moteurs qui interprètent les échappements par barre oblique inverse** (MySQL, ClickHouse, Snowflake, Redshift…) : les barres obliques inverses et les guillemets sont échappés comme ces moteurs les lisent.
  - **Sources inconnues** (une grille issue de plusieurs bases de données) : elles reçoivent une forme sûre sur n'importe quel moteur. La nouvelle option **Cible lisant les chaînes de façon standard** écrit leurs barres obliques inverses telles quelles.
- **Export CSV et TSV :** la protection contre les formules couvre aussi le texte après une `,` ou un `;`, qu'un tableur avec un autre séparateur de liste scinderait dans sa propre cellule.
- **Renommer sur Snowflake :** les vues, fonctions et procédures qui citent l'objet renommé sont listées pour que vous les corrigiez à la main au lieu d'être recréées, afin de conserver leur propriétaire, `EXECUTE AS` et `SECURE`.

## [0.1.9] - 2026-10-09

### Nouveautés
- **Renommer avec impact :** **Renommer…** dans l'explorateur change le nom d'une table, vue, routine, colonne, index ou schéma et, dans le même script, réécrit les vues, procédures, fonctions et déclencheurs qui l'utilisent. Avant d'exécuter, il montre ce que le moteur met à jour seul, ce qui est réécrit et ce qu'il faut vérifier à la main (SQL dynamique, code illisible), avec le script complet. Il s'exécute dans une transaction lorsque le moteur le permet. Il est disponible dans tous les moteurs qui peuvent renommer quelque chose ; les limites de chacun figurent dans `docs/engine-support.md`.
- **Modifier une table :** **Modifier…** ouvre le concepteur sur une table existante et construit l'`ALTER` du moteur. Il conserve ce que le concepteur n'affiche pas (CHECK, options d'index, ordre des colonnes de la clé) et recrée les vues et déclencheurs qui dépendent de la table. Renommer une colonne à cet endroit passe par la revue d'impact ; sur les connexions de production, il demande de saisir le nom de la table avant d'exécuter.
- **Historique par requête :** la barre **Historique** suit l'onglet actif, comme une chronologie : versions de la requête enregistrée avec différences et restauration, ses exécutions et, dans les fichiers d'un projet, ses commits git.
- **Navigation dans l'éditeur :** Cmd/Ctrl+clic sur une table, une vue ou une routine ouvre sa structure ou sa définition, et **Afficher dans l'explorateur** la localise dans l'arborescence. Les tables et colonnes qui n'existent pas sont signalées avant l'exécution.
- **Paramètres dans les requêtes :** `:nom` et `?` sont demandés à l'exécution, et la dernière valeur est mémorisée.
- **Snippets** par moteur (par exemple, `sel` + Tab) et menu du clic droit dans l'éditeur.
- **Totaux de la sélection :** en sélectionnant des cellules de la grille, on voit le nombre, la somme, la moyenne, le minimum et le maximum.
- **Tâches planifiées :** scripts, exports, comparaison de schémas, sauvegardes, **Documenter la base** et **Envoyer un e-mail** (SMTP) qui s'exécutent avec DBine fermé, via le planificateur du système. Avec des notifications par tâche et un historique des exécutions. Ce qui modifie des données est approuvé explicitement.
- **Qualité du code :** règles par moteur dans l'éditeur et **Voir les problèmes**.
- **Documenter la base :** dictionnaire de données en HTML ou Markdown, avec diagramme, lignes estimées et commentaires des vues et routines. Les lignes estimées et les commentaires proviennent des métadonnées du moteur, sans lire les tables ni consommer de quota sur les moteurs cloud.
- **Concevoir une requête :** constructeur visuel de requêtes.
- **Copier un sous-ensemble** de données, avec masquage.
- **Optimiser la requête :** réécritures, index suggérés, alternatives de l'IA et comparaison mesurée. Les alternatives de l'IA sont validées par rapport au plan estimé de la base avant d'être affichées.
- **Contrôle de santé** d'une base, dans tous les moteurs, avec des vérifications propres pour SQL Server, la famille PostgreSQL, la famille MySQL, Oracle, SAP HANA, Firebird, ClickHouse, Snowflake, BigQuery, Databricks et les profils ODBC.
- **Rechercher dans la base :** noms d'objets, code des vues et routines, et noms de colonnes (avec leur table et leur type).
- **Générer des données de test** pour une table.
- **Propriétés de la base** et options avancées à la création d'une base, en onglets et par moteur, avec aperçu du script.
- **Vue JSON en arbre** des résultats, avec édition, et **Ajouter une ligne** / **Ajouter un document** dans l'onglet Données et dans la grille.
- **Nouvelle identité :** le logotype avec le halo.

### Améliorations
- La couleur de la connexion s'affiche comme une bande sur le bord de la ligne, et le point n'indique que l'état (vert connectée, rouge déconnectée).

### Corrections
- La comparaison de schémas n'est plus annulée par les lectures de l'explorateur, et SQL Server se reconnecte.
- Les lignes de connexion sans couleur sont alignées avec celles qui en ont une.
- Faire glisser des tables dans le constructeur de requêtes fonctionne sur macOS.
- Propriétés de SQL Server : les noms de fichier longs ne débordent plus de la boîte de dialogue, et l'onglet « Options ANSI et de sécurité » est traduit.
- L'éditeur ne signale plus comme inconnues les colonnes d'une sous-requête avec alias.

### Déjà disponible
- **Exécuter une requête sur plusieurs bases à la fois :** on choisit une ou plusieurs bases d'une connexion, et les résultats sont réunis avec une colonne indiquant la base de chaque ligne. Arrivé dans la 0.1.4. Voir `docs/multi-database-queries.md`.

## [0.1.8] - 2026-10-06

### Nouveautés
- **PostgreSQL derrière des passerelles qui n'acceptent que le protocole simple :** la connexion a une nouvelle option, **Protocole de requêtes** : Automatique ou Protocole simple uniquement. Elle sert pour les passerelles qui rejettent le protocole étendu avec l'erreur 0A000. Dans ce mode, ce qui nécessite le protocole étendu affiche un message clair au lieu d'échouer.

### Corrections
- **Nouvelle requête avec l'onglet Nouvelle connexion ouvert** échouait avec « FOREIGN KEY constraint failed ». Désormais la requête s'ouvre sur la dernière connexion que vous aviez ouverte, ou vous demande de choisir une base dans l'explorateur.

## [0.1.7] - 2026-10-05

### Corrections
- **Comparer les données avec des colonnes identity :** synchroniser des lignes vers une table SQL Server avec une colonne `IDENTITY` échouait avec « Cannot insert explicit value for identity column ». Désormais DBine active `IDENTITY_INSERT` uniquement pendant l'insertion de ces lignes.
- Dans PostgreSQL, après avoir copié des lignes avec leurs ids, la séquence avance pour que le prochain insert n'entre pas en conflit avec un id copié.

## [0.1.6] - 2026-10-04

### Nouveautés
- **Processus dans le Moniteur :** à côté du tableau de bord, l'onglet **Processus** liste les sessions et les requêtes en cours du serveur, avec des filtres. On peut y annuler une requête ou terminer une session. Disponible dans tous les moteurs qui l'exposent : SQL Server, PostgreSQL, MySQL, Oracle, MongoDB, Redis et la plupart des autres.
- **Authentification Windows dans SQL Server :** avec l'utilisateur actuel (SSPI sous Windows, Kerberos sous macOS et Linux) ou avec un utilisateur et un mot de passe de domaine, aussi depuis Mac et Linux.
- **Kerberos dans MongoDB.**

### Améliorations
- Dans ODBC, les attributs supplémentaires de la connexion remplacent ceux du modèle.
- L'assistant IA a sa propre icône et n'est plus confondu avec **Formater**.
- Les requêtes faites par l'assistant s'affichent traduites dans toutes les langues.
- La télémétrie anonyme compte aussi l'utilisation de l'assistant, du serveur MCP, des synchronisations, des migrations et des requêtes sur plusieurs bases. Jamais de noms, de requêtes ni de données ; elle se désactive dans Paramètres › Général.

## [0.1.5] - 2026-10-03

### Nouveautés
- **L'assistant IA lit votre base, avec votre approbation :** avec un modèle local, il peut consulter la structure et l'utilisation des index de la connexion (par exemple, « analyse les index et dis-moi lequel est en trop »). Avant de lire des lignes ou d'exécuter une requête, il vous montre le SQL exact et la base, avec Approuver ou Refuser. Il ne modifie jamais les données ni la structure.

### Améliorations
- **Arrêter** interrompt la réponse de l'assistant à tout moment et **Nouvelle conversation** est toujours disponible.
- L'option « structure » du chat n'est plus nécessaire : l'assistant demande les détails quand il en a besoin.
- Le curseur de texte apparaît là où l'on peut sélectionner ou saisir.

## [0.1.4] - 2026-10-03

### Nouveautés
- **Projets :** dépôts Git de SQL liés à vos connexions, depuis la deuxième icône de la barre latérale. Arborescence de fichiers, base active ou environnements (dev/qa/prod) sans identifiants dans le dépôt, modifications avec diff, commit, pull et push. Chaque base affiche dans l'Explorateur les projets liés.
- **Exécuter une requête sur plusieurs bases à la fois :** la même requête sur plusieurs bases d'une connexion, avec les résultats réunis et une colonne indiquant la base.
- **DBine se met à jour tout seul :** il télécharge la nouvelle version, vérifie sa signature et redémarre (il demande d'abord s'il y a des tâches en arrière-plan). La 0.1.4 s'installe à la main pour la dernière fois. Sous Linux, cela fonctionne avec l'AppImage ; avec .deb/.rpm, il continue de proposer le téléchargement.
- **Désactiver et activer des index** depuis l'explorateur et l'onglet Index, dans les moteurs qui le permettent (SQL Server, MySQL, MariaDB, TiDB, Oracle, Firebird, CockroachDB, MongoDB…).
- **Sélection de cellules dans la grille :** un bloc (en faisant glisser, Shift+clic ou Shift+flèches) pour le copier, ou des cellules et lignes non contiguës avec Cmd/Ctrl+clic.
- **Comparaison de schémas :** elle peut supprimer un élément à gauche, à droite ou des deux côtés et, avant d'exécuter, montre ce qui en dépend.

### Améliorations
- **Assistant IA :** recommande un modèle intégré plus grand selon la mémoire de votre ordinateur, connaît les particularités de chaque dialecte, réessaie s'il refuse de répondre et conserve l'historique des conversations dans un panneau.
- Le panneau des Tâches a **Retirer les terminées** en haut et se ferme avec Échap ou un clic à l'extérieur.
- Azure SQL Database (aussi Hyperscale) : connecté à master, il liste toutes les bases du serveur.
- CockroachDB : les index s'affichent comme BTREE et GIN, comme dans PostgreSQL.
- Le texte du chat IA peut être sélectionné et copié.

### Corrections
- La barre de la requête ne se désorganise plus à l'ouverture du panneau IA.
- La synchronisation de schémas de libSQL n'échoue plus à cause d'une instruction `PRAGMA` que le serveur rejette.

## [0.1.3] - 2026-10-02

### Nouveautés
- **Plusieurs fenêtres** dans la même instance : **Nouvelle fenêtre** depuis le Dock, la barre des tâches, Fichier › Nouvelle fenêtre ou Cmd/Ctrl+Shift+N. Les connexions, requêtes enregistrées et paramètres sont partagés entre les fenêtres.
- **Tâches en arrière-plan :** les opérations longues (synchroniser des schémas ou des données, sauvegardes, générer des scripts, importer, exporter, cloner des tables, supprimer des objets) peuvent continuer en arrière-plan. Le panneau des Tâches affiche la progression, le temps écoulé, le temps restant estimé et Annuler, et prévient à la fin. À la fermeture de l'application avec des tâches en cours, il demande confirmation avant de les annuler.
- **Utilisation des index** dans tous les moteurs qui la fournissent : clés PK et FK sur les colonnes, dossier Index, pourcentage de lectures par index avec une couleur selon les seeks et les scans, et suppression d'un index depuis l'explorateur.
- **Voir les dépendances… :** ce qui dépend d'une table, colonne, vue ou routine.

### Améliorations
- La synchronisation des données applique chaque côté dans une seule transaction.
- Autocomplétion SQL après « schéma. » et « table. ».
- La comparaison de schémas synchronise les commentaires, a des flèches réversibles et une liste redimensionnable.

### Corrections
- La synchronisation de schémas supprime les clés étrangères en double une par une et, dans SQL Server, change en toute sécurité l'index clustered d'une table.

## [0.1.2] - 2026-10-01

### Nouveautés
- **Exécution de scripts comme dans l'outil de chaque moteur :** instruction par instruction, avec `GO` / `GO N`, `DELIMITER`, `/` et `SET TERM`. Option **Continuer en cas d'erreur**, messages en direct ordonnés, erreurs avec code et ligne, et exécution de l'instruction sous le curseur.
- **Transactions Auto/Manuel** avec Valider et Annuler, et confirmation avant un UPDATE ou DELETE sans WHERE.
- **Schémas :** créer et supprimer des schémas avec propriétaire et permissions ; les schémas vides apparaissent dans l'explorateur.
- **Avis de nouvelle version :** DBine prévient quand une nouvelle version est disponible, à l'ouverture et depuis Aide › Rechercher des mises à jour….
- Réorganiser les connexions et les dossiers par glisser-déposer.
- Supprimer des lignes depuis la grille de données et enregistrer les modifications avec Cmd/Ctrl+S.

### Améliorations
- Annuler une requête conserve la session.

### Corrections
- Le pilote Solr a été republié (il partage du code avec celui d'Elasticsearch).

## [0.1.1] - 2026-09-30

### Nouveautés
- **Télémétrie anonyme**, activée par défaut, avec un avis la première fois. Elle se désactive dans Paramètres ou avec `DO_NOT_TRACK` / `DBINE_TELEMETRY=0`.
- PostgreSQL : options d'identité (colonnes identity) lors de la conception de tables.

### Améliorations
- Oracle : les définitions incluent les index.
- Onglet de définition complète, avec des messages d'erreur de migration plus clairs.
- La comparaison de schémas conserve les lignes et synchronise en une seule étape.
- L'onglet de comparaison de données mémorise vos sélections.
- Pilotes PostgreSQL et Oracle mis à jour en 0.1.2.

## [0.1.0] - 2026-09-30

### Nouveautés
- Première version de DBine, avec des installeurs pour Windows, macOS (Apple Silicon et Intel) et Linux. Les pilotes de chaque moteur, sauf SQLite, se téléchargent à la première connexion.
