; "Open with DBine" for file types other apps own (src-tauri/tauri.windows.conf.json
; registers DBine as the handler only for database files and Parquet).
;
; Tauri's fileAssociations make the app the type's default handler (they
; set the extension's default class). For CSV, TSV, JSON, Excel and the
; generic .db that would take them from Excel, an editor or another app, so
; here DBine only adds itself to the extension's OpenWithProgids: it shows
; in "Open with", and the default stays as it was.

!macro DBINE_OPEN_WITH EXT
  WriteRegStr SHCTX "Software\Classes\.${EXT}\OpenWithProgids" "DBine.datafile" ""
!macroend

!macro DBINE_NOT_OPEN_WITH EXT
  DeleteRegValue SHCTX "Software\Classes\.${EXT}\OpenWithProgids" "DBine.datafile"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  WriteRegStr SHCTX "Software\Classes\DBine.datafile" "" "DBine"
  WriteRegStr SHCTX "Software\Classes\DBine.datafile\DefaultIcon" "" "$INSTDIR\${MAINBINARYNAME}.exe,0"
  WriteRegStr SHCTX "Software\Classes\DBine.datafile\shell\open\command" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\""
  !insertmacro DBINE_OPEN_WITH "csv"
  !insertmacro DBINE_OPEN_WITH "tsv"
  !insertmacro DBINE_OPEN_WITH "json"
  !insertmacro DBINE_OPEN_WITH "jsonl"
  !insertmacro DBINE_OPEN_WITH "ndjson"
  !insertmacro DBINE_OPEN_WITH "xlsx"
  !insertmacro DBINE_OPEN_WITH "db"
  System::Call "shell32::SHChangeNotify(i 0x08000000, i 0x1000, p 0, p 0)"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  !insertmacro DBINE_NOT_OPEN_WITH "csv"
  !insertmacro DBINE_NOT_OPEN_WITH "tsv"
  !insertmacro DBINE_NOT_OPEN_WITH "json"
  !insertmacro DBINE_NOT_OPEN_WITH "jsonl"
  !insertmacro DBINE_NOT_OPEN_WITH "ndjson"
  !insertmacro DBINE_NOT_OPEN_WITH "xlsx"
  !insertmacro DBINE_NOT_OPEN_WITH "db"
  DeleteRegKey SHCTX "Software\Classes\DBine.datafile"
  System::Call "shell32::SHChangeNotify(i 0x08000000, i 0x1000, p 0, p 0)"
!macroend
