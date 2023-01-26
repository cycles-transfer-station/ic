use std::{cell::RefCell, cmp::Reverse, mem::size_of, thread::LocalKey, time::Duration};

use candid::{CandidType, Deserialize};
use certificate_orchestrator_interface::{
    CreateRegistrationError, CreateRegistrationResponse, DispenseTaskError, DispenseTaskResponse,
    EncryptedPair, ExportCertificatesError, ExportCertificatesResponse, GetRegistrationError,
    GetRegistrationResponse, Id, ListAllowedPrincipalsError, ListAllowedPrincipalsResponse,
    ModifyAllowedPrincipalError, ModifyAllowedPrincipalResponse, Name, QueueTaskError,
    QueueTaskResponse, Registration, State, UpdateRegistrationError, UpdateRegistrationResponse,
    UploadCertificateError, UploadCertificateResponse, NAME_MAX_LEN,
};
use ic_cdk::{
    api::time, caller, export::Principal, post_upgrade, pre_upgrade, timer::set_timer_interval,
    trap,
};
use ic_cdk_macros::{init, query, update};
use ic_stable_structures::{
    memory_manager::{MemoryId, MemoryManager, VirtualMemory},
    DefaultMemoryImpl, StableBTreeMap,
};
use priority_queue::PriorityQueue;

use crate::{
    acl::{Authorize, AuthorizeError, Authorizer, WithAuthorize},
    certificate::{Export, ExportError, Exporter, Upload, UploadError, Uploader},
    id::{Generate, Generator},
    registration::{
        Create, CreateError, Creator, Expire, Expirer, Get, GetError, Getter, Update, UpdateError,
        Updater,
    },
    work::{Dispense, DispenseError, Dispenser, Queue, QueueError, Queuer, Retrier, Retry},
};

mod acl;
mod certificate;
mod id;
mod persistence;
mod registration;
mod work;

type Memory = VirtualMemory<DefaultMemoryImpl>;
type LocalRef<T> = &'static LocalKey<RefCell<T>>;
type StableSet<T> = StableBTreeMap<Memory, T, ()>;
type StableValue<T> = StableBTreeMap<Memory, (), T>;

const BYTE: u32 = 1;
const KB: u32 = 1024 * BYTE;

const CONST_KEY_LEN: u32 = 0;
const SET_VALUE_LEN: u32 = 0;

const PRINCIPAL_ID_LEN: u32 = 63 * BYTE;
const ID_COUNTER_LEN: u32 = size_of::<u128>() as u32;
const ID_SEED_LEN: u32 = size_of::<u128>() as u32;
const REGISTRATION_ID_LEN: u32 = 64 * BYTE;
const REGISTRATION_LEN: u32 = 128;
const ENCRYPTED_PRIVATE_KEY_LEN: u32 = KB; // 1 * KB
const ENCRYPTED_CERTIFICATE_LEN: u32 = 8 * KB;
const ENCRYPTED_PAIR_LEN: u32 = ENCRYPTED_PRIVATE_KEY_LEN + ENCRYPTED_CERTIFICATE_LEN;

const REGISTRATION_EXPIRATION_TTL: Duration = Duration::from_secs(6 * 3600); // 6 Hours
const IN_PROGRESS_TTL: Duration = Duration::from_secs(10 * 60); // 10 Minutes

// Memory
thread_local! {
    static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));
}

const MEMORY_ID_ROOT_PRINCIPALS: u8 = 0;
const MEMORY_ID_ALLOWED_PRINCIPALS: u8 = 1;
const MEMORY_ID_ID_COUNTER: u8 = 2;
const MEMORY_ID_ID_SEED: u8 = 3;
const MEMORY_ID_REGISTRATIONS: u8 = 4;
const MEMORY_ID_NAMES: u8 = 5;
const MEMORY_ID_ENCRYPTED_CERTIFICATES: u8 = 6;
const MEMORY_ID_TASKS: u8 = 7;
const MEMORY_ID_EXPIRATIONS: u8 = 8;
const MEMORY_ID_RETRIES: u8 = 9;

// ACLs
thread_local! {
    static ROOT_PRINCIPALS: RefCell<StableSet<String>> = RefCell::new(
        StableSet::init(
            MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(MEMORY_ID_ROOT_PRINCIPALS))),
            PRINCIPAL_ID_LEN, // MAX_KEY_SIZE,
            SET_VALUE_LEN,    // MAX_VALUE_SIZE
        )
    );

    static ROOT_AUTHORIZER: RefCell<Box<dyn Authorize>> = RefCell::new({
        let a = Authorizer::new(&ROOT_PRINCIPALS);
        Box::new(a)
    });
}

thread_local! {
    static ALLOWED_PRINCIPALS: RefCell<StableSet<String>> = RefCell::new(
        StableSet::init(
            MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(MEMORY_ID_ALLOWED_PRINCIPALS))),
            PRINCIPAL_ID_LEN, // MAX_KEY_SIZE,
            SET_VALUE_LEN,    // MAX_VALUE_SIZE
        )
    );

    static MAIN_AUTHORIZER: RefCell<Box<dyn Authorize>> = RefCell::new({
        let a = Authorizer::new(&ALLOWED_PRINCIPALS);
        Box::new(a)
    });
}

// ID Generation
thread_local! {
    static ID_COUNTER: RefCell<StableValue<u128>> = RefCell::new(
        StableValue::init(
            MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(MEMORY_ID_ID_COUNTER))),
            CONST_KEY_LEN,  // MAX_KEY_SIZE,
            ID_COUNTER_LEN, // MAX_VALUE_SIZE
        )
    );

    static ID_SEED: RefCell<StableValue<u128>> = RefCell::new(
        StableValue::init(
            MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(MEMORY_ID_ID_SEED))),
            CONST_KEY_LEN,  // MAX_KEY_SIZE,
            ID_SEED_LEN,    // MAX_VALUE_SIZE
        )
    );

    static ID_GENERATOR: RefCell<Box<dyn Generate>> = RefCell::new({
        let g = Generator::new(&ID_COUNTER, &ID_SEED);
        Box::new(g)
    });
}

// Registrations
thread_local! {
    static REGISTRATIONS: RefCell<StableBTreeMap<Memory, Id, Registration>> = RefCell::new(
        StableBTreeMap::init(
            MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(MEMORY_ID_REGISTRATIONS))),
            REGISTRATION_ID_LEN, // MAX_KEY_SIZE,
            REGISTRATION_LEN,    // MAX_VALUE_SIZE
        )
    );

    static NAMES: RefCell<StableBTreeMap<Memory, Name, Id>> = RefCell::new(
        StableBTreeMap::init(
            MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(MEMORY_ID_NAMES))),
            NAME_MAX_LEN,        // MAX_KEY_SIZE,
            REGISTRATION_ID_LEN, // MAX_VALUE_SIZE
        )
    );

    static EXPIRATIONS: RefCell<PriorityQueue<Id, Reverse<u64>>> = RefCell::new(PriorityQueue::new());

    static RETRIES: RefCell<PriorityQueue<Id, Reverse<u64>>> = RefCell::new(PriorityQueue::new());

    static CREATOR: RefCell<Box<dyn Create>> = RefCell::new({
        let c = Creator::new(&ID_GENERATOR, &REGISTRATIONS, &NAMES, &EXPIRATIONS);
        let c = WithAuthorize(c, &MAIN_AUTHORIZER);
        Box::new(c)
    });

    static GETTER: RefCell<Box<dyn Get>> = RefCell::new({
        let g = Getter::new(&REGISTRATIONS);
        let g = WithAuthorize(g, &MAIN_AUTHORIZER);
        Box::new(g)
    });

    static UPDATER: RefCell<Box<dyn Update>> = RefCell::new({
        let u = Updater::new(&REGISTRATIONS, &EXPIRATIONS, &RETRIES);
        let u = WithAuthorize(u, &MAIN_AUTHORIZER);
        Box::new(u)
    });
}

// Certificates
thread_local! {
    static ENCRYPTED_CERTIFICATES: RefCell<StableBTreeMap<Memory, Id, EncryptedPair>> = RefCell::new(
        StableBTreeMap::init(
            MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(MEMORY_ID_ENCRYPTED_CERTIFICATES))),
            REGISTRATION_ID_LEN, // MAX_KEY_SIZE,
            ENCRYPTED_PAIR_LEN,  // MAX_VALUE_SIZE
        )
    );

    static UPLOADER: RefCell<Box<dyn Upload>> = RefCell::new({
        let u = Uploader::new(&ENCRYPTED_CERTIFICATES, &REGISTRATIONS);
        let u = WithAuthorize(u, &MAIN_AUTHORIZER);
        Box::new(u)
    });

    static EXPORTER: RefCell<Box<dyn Export>> = RefCell::new({
        let e = Exporter::new(&ENCRYPTED_CERTIFICATES, &REGISTRATIONS);
        let e = WithAuthorize(e, &MAIN_AUTHORIZER);
        Box::new(e)
    });
}

// Tasks

thread_local! {
    static TASKS: RefCell<PriorityQueue<Id, Reverse<u64>>> = RefCell::new(PriorityQueue::new());

    static QUEUER: RefCell<Box<dyn Queue>> = RefCell::new({
        let q = Queuer::new(&TASKS, &REGISTRATIONS);
        let q = WithAuthorize(q, &MAIN_AUTHORIZER);
        Box::new(q)
    });

    static DISPENSER: RefCell<Box<dyn Dispense>> = RefCell::new({
        let d = Dispenser::new(&TASKS, &RETRIES);
        let d = WithAuthorize(d, &MAIN_AUTHORIZER);
        Box::new(d)
    });
}

// Expirations and retries

thread_local! {
    static EXPIRER: RefCell<Box<dyn Expire>> = RefCell::new({
        let e = Expirer::new(&REGISTRATIONS, &NAMES, &TASKS, &EXPIRATIONS);
        Box::new(e)
    });

    static RETRIER: RefCell<Box<dyn Retry>> = RefCell::new({
        let r = Retrier::new(&TASKS, &RETRIES);
        Box::new(r)
    });
}

// Timers

fn init_timers_fn() {
    set_timer_interval(
        Duration::from_secs(60), // 1 Minute
        || {
            if let Err(err) = EXPIRER.with(|e| e.borrow().expire(time())) {
                trap(&format!("failed to run expire: {err}"));
            }
        },
    );

    set_timer_interval(
        Duration::from_secs(60), // 1 Minute
        || {
            if let Err(err) = RETRIER.with(|r| r.borrow().retry(time())) {
                trap(&format!("failed to run retry: {err}"));
            }
        },
    );
}

// Init / Upgrade

#[derive(Clone, Debug, CandidType, Deserialize)]
struct InitArg {
    #[serde(rename = "rootPrincipals")]
    root_principals: Vec<Principal>,
    #[serde(rename = "idSeed")]
    id_seed: u128,
}

#[init]
fn init_fn(
    InitArg {
        root_principals,
        id_seed,
    }: InitArg,
) {
    ROOT_PRINCIPALS.with(|m| {
        let mut m = m.borrow_mut();
        root_principals.iter().for_each(|p| {
            if let Err(err) = m.insert(p.to_text(), ()) {
                trap(&format!("failed to insert root principal: {err}"));
            }
        });
    });

    ID_SEED.with(|s| {
        let mut s = s.borrow_mut();
        s.insert((), id_seed).unwrap();
    });

    init_timers_fn();
}

#[pre_upgrade]
fn pre_upgrade_fn() {
    MEMORY_MANAGER.with(|m| {
        let m = m.borrow();

        TASKS.with(|tasks| {
            if let Err(err) = persistence::store(m.get(MemoryId::new(MEMORY_ID_TASKS)), tasks) {
                trap(&format!("failed to persist tasks: {err}"));
            }
        });

        EXPIRATIONS.with(|exps| {
            if let Err(err) = persistence::store(m.get(MemoryId::new(MEMORY_ID_EXPIRATIONS)), exps)
            {
                trap(&format!("failed to persist expirations: {err}"));
            }
        });

        RETRIES.with(|retries| {
            if let Err(err) = persistence::store(m.get(MemoryId::new(MEMORY_ID_RETRIES)), retries) {
                trap(&format!("failed to persist retries: {err}"));
            }
        });
    });
}

#[post_upgrade]
fn post_upgrade_fn() {
    MEMORY_MANAGER.with(|m| {
        let m = m.borrow();

        TASKS.with(|tasks| {
            match persistence::load(m.get(MemoryId::new(MEMORY_ID_TASKS))) {
                Ok(v) => *tasks.borrow_mut() = v,
                Err(err) => trap(&format!("failed to load tasks: {err}")),
            };
        });

        EXPIRATIONS.with(|exps| {
            match persistence::load(m.get(MemoryId::new(MEMORY_ID_EXPIRATIONS))) {
                Ok(v) => *exps.borrow_mut() = v,
                Err(err) => trap(&format!("failed to load expirations: {err}")),
            };
        });

        RETRIES.with(|retries| {
            match persistence::load(m.get(MemoryId::new(MEMORY_ID_RETRIES))) {
                Ok(v) => *retries.borrow_mut() = v,
                Err(err) => trap(&format!("failed to load retries: {err}")),
            };
        });
    });

    init_timers_fn();
}

// Registration

#[update(name = "createRegistration")]
fn create_registration(name: String, canister: Principal) -> CreateRegistrationResponse {
    match CREATOR.with(|c| c.borrow().create(&name, &canister)) {
        Ok(id) => CreateRegistrationResponse::Ok(id),
        Err(err) => CreateRegistrationResponse::Err(match err {
            CreateError::Duplicate(id) => CreateRegistrationError::Duplicate(id),
            CreateError::NameError(err) => CreateRegistrationError::NameError(err.to_string()),
            CreateError::Unauthorized => CreateRegistrationError::Unauthorized,
            CreateError::UnexpectedError(err) => {
                CreateRegistrationError::UnexpectedError(err.to_string())
            }
        }),
    }
}

#[query(name = "getRegistration")]
fn get_registration(id: Id) -> GetRegistrationResponse {
    match GETTER.with(|g| g.borrow().get(&id)) {
        Ok(reg) => GetRegistrationResponse::Ok(reg),
        Err(err) => GetRegistrationResponse::Err(match err {
            GetError::NotFound => GetRegistrationError::NotFound,
            GetError::Unauthorized => GetRegistrationError::Unauthorized,
            GetError::UnexpectedError(err) => {
                GetRegistrationError::UnexpectedError(err.to_string())
            }
        }),
    }
}

#[update(name = "updateRegistration")]
fn update_registration(id: Id, state: State) -> UpdateRegistrationResponse {
    match UPDATER.with(|u| u.borrow().update(id, state)) {
        Ok(()) => UpdateRegistrationResponse::Ok(()),
        Err(err) => UpdateRegistrationResponse::Err(match err {
            UpdateError::NotFound => UpdateRegistrationError::NotFound,
            UpdateError::Unauthorized => UpdateRegistrationError::Unauthorized,
            UpdateError::UnexpectedError(_) => {
                UpdateRegistrationError::UnexpectedError(err.to_string())
            }
        }),
    }
}

// Certificates

#[update(name = "uploadCertificate")]
fn upload_certificate(id: Id, pair: EncryptedPair) -> UploadCertificateResponse {
    match UPLOADER.with(|u| u.borrow().upload(&id, pair)) {
        Ok(()) => UploadCertificateResponse::Ok(()),
        Err(err) => UploadCertificateResponse::Err(match err {
            UploadError::NotFound => UploadCertificateError::NotFound,
            UploadError::Unauthorized => UploadCertificateError::Unauthorized,
            UploadError::UnexpectedError(_) => {
                UploadCertificateError::UnexpectedError(err.to_string())
            }
        }),
    }
}

#[query(name = "exportCertificates")]
fn export_certificates() -> ExportCertificatesResponse {
    match EXPORTER.with(|e| e.borrow().export()) {
        Ok(pkgs) => ExportCertificatesResponse::Ok(pkgs),
        Err(err) => ExportCertificatesResponse::Err(match err {
            ExportError::Unauthorized => ExportCertificatesError::Unauthorized,
            ExportError::UnexpectedError(_) => {
                ExportCertificatesError::UnexpectedError(err.to_string())
            }
        }),
    }
}

// Tasks

#[update(name = "queueTask")]
fn queue_task(id: Id, timestamp: u64) -> QueueTaskResponse {
    match QUEUER.with(|q| q.borrow().queue(id, timestamp)) {
        Ok(()) => QueueTaskResponse::Ok(()),
        Err(err) => QueueTaskResponse::Err(match err {
            QueueError::NotFound => QueueTaskError::NotFound,
            QueueError::Unauthorized => QueueTaskError::Unauthorized,
            QueueError::UnexpectedError(err) => QueueTaskError::UnexpectedError(err.to_string()),
        }),
    }
}

#[update(name = "dispenseTask")]
fn dispense_task() -> DispenseTaskResponse {
    match DISPENSER.with(|d| d.borrow().dispense()) {
        Ok(id) => DispenseTaskResponse::Ok(id),
        Err(err) => DispenseTaskResponse::Err(match err {
            DispenseError::NoTasksAvailable => DispenseTaskError::NoTasksAvailable,
            DispenseError::Unauthorized => DispenseTaskError::Unauthorized,
            DispenseError::UnexpectedError(err) => {
                DispenseTaskError::UnexpectedError(err.to_string())
            }
        }),
    }
}

#[query(name = "peekTask")]
fn peek_task() -> DispenseTaskResponse {
    match DISPENSER.with(|d| d.borrow().peek()) {
        Ok(id) => DispenseTaskResponse::Ok(id),
        Err(err) => DispenseTaskResponse::Err(match err {
            DispenseError::NoTasksAvailable => DispenseTaskError::NoTasksAvailable,
            DispenseError::Unauthorized => DispenseTaskError::Unauthorized,
            DispenseError::UnexpectedError(err) => {
                DispenseTaskError::UnexpectedError(err.to_string())
            }
        }),
    }
}

// ACLs

#[query(name = "listAllowedPrincipals")]
fn list_allowed_principals() -> ListAllowedPrincipalsResponse {
    if let Err(err) = ROOT_AUTHORIZER.with(|a| a.borrow().authorize(&caller())) {
        return ListAllowedPrincipalsResponse::Err(match err {
            AuthorizeError::Unauthorized => ListAllowedPrincipalsError::Unauthorized,
            AuthorizeError::UnexpectedError(err) => {
                ListAllowedPrincipalsError::UnexpectedError(err.to_string())
            }
        });
    }

    ListAllowedPrincipalsResponse::Ok(ALLOWED_PRINCIPALS.with(|m| {
        m.borrow()
            .iter()
            .map(|(k, _)| Principal::from_text(k).unwrap())
            .collect()
    }))
}

#[update(name = "addAllowedPrincipal")]
fn add_allowed_principal(principal: Principal) -> ModifyAllowedPrincipalResponse {
    if let Err(err) = ROOT_AUTHORIZER.with(|a| a.borrow().authorize(&caller())) {
        return ModifyAllowedPrincipalResponse::Err(match err {
            AuthorizeError::Unauthorized => ModifyAllowedPrincipalError::Unauthorized,
            AuthorizeError::UnexpectedError(err) => {
                ModifyAllowedPrincipalError::UnexpectedError(err.to_string())
            }
        });
    }

    if let Err(err) = ALLOWED_PRINCIPALS.with(|m| m.borrow_mut().insert(principal.to_text(), ())) {
        return ModifyAllowedPrincipalResponse::Err(ModifyAllowedPrincipalError::UnexpectedError(
            err.to_string(),
        ));
    }
    ModifyAllowedPrincipalResponse::Ok(())
}

#[update(name = "rmAllowedPrincipal")]
fn rm_allowed_principal(principal: Principal) -> ModifyAllowedPrincipalResponse {
    if let Err(err) = ROOT_AUTHORIZER.with(|a| a.borrow().authorize(&caller())) {
        return ModifyAllowedPrincipalResponse::Err(match err {
            AuthorizeError::Unauthorized => ModifyAllowedPrincipalError::Unauthorized,
            AuthorizeError::UnexpectedError(err) => {
                ModifyAllowedPrincipalError::UnexpectedError(err.to_string())
            }
        });
    }

    ALLOWED_PRINCIPALS.with(|m| m.borrow_mut().remove(&principal.to_text()));

    ModifyAllowedPrincipalResponse::Ok(())
}
