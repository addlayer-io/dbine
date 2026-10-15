# Modifications

## [0.1.13] - 2026-10-16

### Améliorations
- **Soutenir le projet :** le bouton en forme de cœur (**Soutenir le projet**), son rappel et **Aide › Soutenir le projet** ouvrent Buy Me a Coffee.
- **Script avec les données de tables choisies :** dans **Générer le script de la base**, lorsque **Données (INSERT)** est coché, vous pouvez garder les données de toutes les tables ou choisir lesquelles en reçoivent, depuis une liste avec filtre (**Choisir des tables…**), de sorte que le script puisse contenir la structure de toute la base et les lignes de quelques tables seulement.

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
