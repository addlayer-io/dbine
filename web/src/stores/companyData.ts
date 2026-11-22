// What goes from the window when DBine Enterprise's access ends (the backend
// deletes its own stores; this is the part that lives in the UI).

/** A DBine Enterprise (managed) connection's id. */
export const isCompanyId = (id: string | null | undefined): boolean => !!id && id.startsWith('ent-');

interface Tagged { company?: boolean; companyConnection?: string; companyDatabase?: string }

/** Whether a conversation used a company connection. */
export const usedCompany = (messages: Tagged[]): boolean => messages.some((m) => m.company);

/** The archive without the conversations that used a company connection. */
export function withoutCompanyChats<T extends { messages: Tagged[] }>(list: T[]): T[] {
  return list.filter((c) => !usedCompany(c.messages));
}

/** What the person lost of the company when they keep the rest. */
export interface Lost {
  /** Whole managed connections (`ent-…`). */
  connections: string[];
  /** Databases of one connection: these, and every one not in `keep` when it is a list. */
  databases: { connection_id: string; databases: string[]; keep: string[] | null }[];
}

/** Whether `database` of a connection is one that was lost. An unnamed one (the
 *  connection's default) counts: it can be the one that went. */
export function databaseLost(lost: Lost, connectionId: string, database: string): boolean {
  if (lost.connections.includes(connectionId)) return true;
  return lost.databases.some(
    (d) => d.connection_id === connectionId
      && (!database || d.databases.includes(database) || (d.keep !== null && !d.keep.includes(database))),
  );
}

/** Whether a conversation quoted something of what was lost. A conversation
 *  tagged only as "company" (older, without which connection) goes too. */
export function usedLost(messages: Tagged[], lost: Lost): boolean {
  return messages.some((m) => m.company && (
    !m.companyConnection || databaseLost(lost, m.companyConnection, m.companyDatabase ?? '')
  ));
}

/** The archive without the conversations that quoted what was lost. */
export function withoutLostChats<T extends { messages: Tagged[] }>(list: T[], lost: Lost): T[] {
  return list.filter((c) => !usedLost(c.messages, lost));
}
