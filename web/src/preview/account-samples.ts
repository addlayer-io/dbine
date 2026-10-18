// Development only: "Conectar <gestor>" (AccountConnectWizard) filled as the
// user would have it at each step, with what the last step would find
// (view=account-wizard&kind=…&step=…[&result=ok][&error=1][&detect=none|missing]).
import { tb } from '../i18n/backend';
import { STEPS, type WizardForm, type WizardKind } from '../composables/accountWizards';

const UUID = '6f1c2a9e-0d1b-4c55-9a51-2f7d8c3b1e04';

const FILLED: Record<WizardKind, Partial<WizardForm>> = {
  bitwarden: { server: 'bitwarden.eu', client_id: `user.${UUID}`, client_secret: 'dummy-client-secret', passphrase: 'dummy-master' },
  bitwarden_sm: { server: 'bitwarden.com', token: `0.${UUID}.dummySecret:AAECAwQFBgcICQoLDA0ODw==` },
  vault: { address: 'https://vault.example.com:8200', namespace: 'admin/datos', auth: 'approle', role_id: 'dbine', secret_id: 'dummy-secret-id' },
  delinea: { url: 'https://acme.secretservercloud.com', username: 'svc_dbine', domain: 'CORP', password: 'dummy-password' },
  conjur: { url: 'https://conjur.example.com', conjur_account: 'myorg', login: 'host/apps/dbine', api_key: 'dummy-api-key' },
  ccp: { url: 'https://ccp.example.com', app_id: 'DBine', ca_file: '~/certs/ccp-ca.pem', client_cert: '~/certs/dbine.p12', passphrase: 'dummy-p12-pass' },
  onepassword: { op_account: 'my.1password.com' },
  aws: { profile: 'prod', region: 'eu-west-1' },
  azure: { vault_url: 'https://kv-ventas.vault.azure.net', tenant: '11111111-2222-4333-8444-555555555555' },
  gcp: { project: 'ventas-prod' },
  keepass: { file: '~/Passwords/databases.kdbx', passphrase: 'dummy-master' },
};

const FOUND: Record<WizardKind, [string, string]> = {
  bitwarden: ['ana@example.com', ''],
  bitwarden_sm: ['a0a1a2a3-b0b1-4c0c-9d0d-e0e1e2e3e4e5', '12 secretos visibles para la cuenta de máquina'],
  vault: ['approle', 'políticas: dbine-db, default'],
  delinea: ['svc_dbine', ''],
  conjur: ['host/apps/dbine (myorg)', ''],
  ccp: ['DBine', 'conexión TLS con el certificado de cliente aceptada (HTTP 404); el AppID se prueba con «Probar» en una conexión'],
  onepassword: ['ana@example.com', ''],
  aws: ['arn:aws:iam::123456789012:user/ana', ''],
  azure: ['ana@contoso.com', 'inquilino 11111111-2222-4333-8444-555555555555'],
  gcp: ['ana@example.com', ''],
  keepass: ['databases.kdbx', '42 entradas'],
};

const ERRORS: Record<WizardKind, string> = {
  bitwarden: 'la contraseña maestra de Bitwarden no es la correcta',
  bitwarden_sm: 'Bitwarden rechazó el token de acceso de la cuenta de máquina (¿se revocó o es de otro servidor?)',
  vault: 'Vault rechazó el role_id o el secret_id de AppRole (¿venció el secret_id?)',
  delinea: 'Delinea rechazó el usuario o la contraseña (o el ID y el secreto del cliente)',
  conjur: 'Conjur rechazó el login «host/apps/dbine» o su clave de API',
  ccp: 'no se pudo abrir el archivo PKCS#12 (¿la frase de contraseña es la correcta?)',
  onepassword: 'la CLI de 1Password no tiene una sesión iniciada: ejecutá «op signin» o activá la integración de la CLI en la app de 1Password (Ajustes › Desarrollador)',
  aws: 'las credenciales temporales de AWS vencieron: volvé a iniciar sesión (aws sso login)',
  azure: 'el token es del inquilino 99999999-2222-4333-8444-555555555555, no de 11111111-2222-4333-8444-555555555555',
  gcp: 'no se pudo obtener un token de Google Cloud',
  keepass: 'la contraseña maestra o el archivo de clave no abren la base de KeePass',
};

const DETECT: Partial<Record<WizardKind, Record<string, unknown>>> = {
  onepassword: { installed: true, accounts: [{ url: 'my.1password.com', email: 'ana@example.com' }, { url: 'acme.1password.com', email: 'ana@acme.com' }] },
  aws: { profiles: [{ name: 'default', region: 'us-east-1' }, { name: 'prod', region: 'eu-west-1' }, { name: 'sso-datos', region: '' }], region: '' },
  azure: { tenant: '11111111-2222-4333-8444-555555555555', user: 'ana@contoso.com', subscription: 'Producción' },
  gcp: { account: 'ana@example.com', project: 'ventas-prod' },
};

export function accountWizardSample(params: URLSearchParams) {
  const kind = (params.get('kind') ?? 'bitwarden') as WizardKind;
  const last = STEPS[kind].length;
  const step = Math.min(Number(params.get('step') ?? 1), last);
  const result = params.get('result') === 'ok';
  const d = params.get('detect');
  let detect = DETECT[kind];
  if (d === 'none' && kind === 'onepassword') detect = { installed: true, accounts: [] };
  if (d === 'missing') detect = kind === 'onepassword' ? { installed: false } : { error: 'x' };
  // Steps before this one are filled; this one too, unless &empty=1.
  const filled = params.get('empty') === '1' ? {} : FILLED[kind];
  const [identity, detail] = FOUND[kind];
  const preview = {
    ...filled,
    ...(d === 'none' && kind === 'onepassword' ? { op_account: '' } : {}),
    detect,
    result: result ? ('ok' as const) : undefined,
    identity,
    detail,
    error: params.get('error') ? tb(ERRORS[kind]) : undefined,
  };
  return { kind, step: result ? last : step, preview };
}
