import type { SavedSearchLock } from './saved-searches.ts'

const DATABASE_NAME = 'eidos-browser-locks'
const STORE_NAME = 'exclusive-locks'

const databases = new WeakMap<IDBFactory, Promise<IDBDatabase>>()
let localTail: Promise<unknown> = Promise.resolve()

class LockInfrastructureError extends Error {
  readonly actionStarted: boolean

  constructor(actionStarted: boolean, cause: unknown) {
    super('Browser lock infrastructure failed', { cause })
    this.actionStarted = actionStarted
  }
}

function openDatabase(factory: IDBFactory): Promise<IDBDatabase> {
  const existing = databases.get(factory)
  if (existing) return existing

  const opened = new Promise<IDBDatabase>((resolve, reject) => {
    const request = factory.open(DATABASE_NAME, 1)
    request.onupgradeneeded = () => {
      if (!request.result.objectStoreNames.contains(STORE_NAME)) request.result.createObjectStore(STORE_NAME)
    }
    request.onsuccess = () => {
      const database = request.result
      database.onversionchange = () => {
        database.close()
        databases.delete(factory)
      }
      resolve(database)
    }
    request.onerror = () => reject(request.error ?? new Error('Could not open the browser lock database.'))
    request.onblocked = () => reject(new Error('The browser lock database upgrade is blocked.'))
  }).catch((error) => {
    databases.delete(factory)
    throw error
  })

  databases.set(factory, opened)
  return opened
}

/**
 * A read-write IndexedDB transaction is exclusive across tabs for this object
 * store. Run the synchronous localStorage mutation from its first request so
 * another tab cannot enter the same fallback critical section concurrently.
 */
function runInDatabase<T>(database: IDBDatabase, name: string, action: () => T): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    let transaction: IDBTransaction
    try {
      transaction = database.transaction(STORE_NAME, 'readwrite')
    } catch (error) {
      reject(new LockInfrastructureError(false, error))
      return
    }

    let actionStarted = false
    let actionResult: T
    let actionError: unknown
    const request = transaction.objectStore(STORE_NAME).get(name)
    request.onsuccess = () => {
      actionStarted = true
      try {
        actionResult = action()
        transaction.objectStore(STORE_NAME).put(Date.now(), name)
      } catch (error) {
        actionError = error
        transaction.abort()
      }
    }
    transaction.oncomplete = () => resolve(actionResult)
    transaction.onabort = () => {
      reject(actionError ?? new LockInfrastructureError(actionStarted, transaction.error))
    }
  })
}

function runLocally<T>(action: () => T): Promise<T> {
  const result = localTail.then(action, action)
  localTail = result.then(
    () => undefined,
    () => undefined,
  )
  return result
}

export function createBrowserLock(
  webLocks: LockManager | undefined = typeof navigator === 'undefined' ? undefined : navigator.locks,
  indexedDb: IDBFactory | undefined = globalThis.indexedDB,
): SavedSearchLock {
  return {
    async run(name, action) {
      if (webLocks) return webLocks.request(name, action)
      if (!indexedDb) return runLocally(action)

      let database: IDBDatabase
      try {
        database = await openDatabase(indexedDb)
      } catch {
        return runLocally(action)
      }

      try {
        return await runInDatabase(database, name, action)
      } catch (error) {
        if (!(error instanceof LockInfrastructureError) || error.actionStarted) throw error
        databases.delete(indexedDb)
        database.close()
        return runLocally(action)
      }
    },
  }
}
