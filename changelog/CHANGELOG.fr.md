# Modifications

## [0.1.9] - 2026-10-09

### Nouveautés
- **Renommer avec impact :** **Renommer…** dans l'explorateur change le nom d'une table, vue, routine, colonne, index ou schéma et, dans le même script, réécrit les vues, procédures, fonctions et déclencheurs qui l'utilisent. Avant d'exécuter, il montre ce que le moteur met à jour seul, ce qui est réécrit et ce qu'il faut vérifier à la main (SQL dynamique, code illisible), avec le script complet. Il s'exécute dans une transaction lorsque le moteur le permet. Il est disponible dans tous les moteurs qui peuvent renommer quelque chose ; les limites de chacun figurent dans `docs/soporte-por-motor.md`.
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
- **Exécuter une requête sur plusieurs bases à la fois :** on choisit une ou plusieurs bases d'une connexion, et les résultats sont réunis avec une colonne indiquant la base de chaque ligne. Arrivé dans la 0.1.4. Voir `docs/ejecutar-en-varias-bases.md`.

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
