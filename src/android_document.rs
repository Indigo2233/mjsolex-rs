use std::fs::File;
use std::io;
use std::os::fd::BorrowedFd;
use std::sync::Arc;

use jni::errors::Result as JniResult;
use jni::objects::{GlobalRef, JObject, JValue};
use jni::{JNIEnv, JavaVM};

/// The duplicated data fd and its original Android descriptor. Reliable descriptors
/// carry a separate status channel that the original object must close after the I/O.
pub(crate) struct DocumentFile {
    pub(crate) file: File,
    descriptor: Option<Arc<Descriptor>>,
}

impl DocumentFile {
    pub(crate) fn from_android(
        env: &mut JNIEnv<'_>,
        object: JObject<'_>,
        export: bool,
    ) -> io::Result<Self> {
        let fd = env
            .call_method(&object, "getFd", "()I", &[])
            .and_then(|value| value.i())
            .map_err(io::Error::other)?;
        if fd < 0 {
            return Err(io::Error::other("Android handed over a closed document"));
        }
        // SAFETY: the Java caller keeps this ParcelFileDescriptor open until
        // it receives the callback's ownership-transfer result.
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        let owned = fd.try_clone_to_owned()?;
        let vm = env.get_java_vm().map_err(io::Error::other)?;
        let object = env.new_global_ref(object).map_err(io::Error::other)?;
        Ok(Self {
            file: File::from(owned),
            descriptor: Some(Arc::new(Descriptor {
                vm,
                object: Some(object),
                export,
            })),
        })
    }

    pub(crate) fn cached(file: File) -> Self {
        Self {
            file,
            descriptor: None,
        }
    }

    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            descriptor: self.descriptor.clone(),
        })
    }

    pub(crate) fn check_error(&self) -> io::Result<()> {
        if let Some(descriptor) = &self.descriptor
            && let Some(object) = &descriptor.object
        {
            descriptor.call(object, "checkError", None)?;
        }
        Ok(())
    }

    pub(crate) fn finish_export(self, failed: bool) -> io::Result<()> {
        let Self { file, descriptor } = self;
        drop(file);
        let mut descriptor = descriptor
            .and_then(Arc::into_inner)
            .ok_or_else(|| io::Error::other("Expected an exclusively owned export document"))?;
        descriptor.close(failed)
    }
}

struct Descriptor {
    vm: JavaVM,
    object: Option<GlobalRef>,
    export: bool,
}

impl Descriptor {
    fn call(&self, object: &GlobalRef, method: &str, message: Option<&str>) -> io::Result<()> {
        let mut env = self.vm.attach_current_thread().map_err(io::Error::other)?;
        let result: JniResult<()> = env.with_local_frame(4, |env| {
            if let Some(message) = message {
                let message = env.new_string(message)?;
                env.call_method(
                    object,
                    method,
                    "(Ljava/lang/String;)V",
                    &[JValue::Object(&message)],
                )?;
            } else {
                env.call_method(object, method, "()V", &[])?;
            }
            Ok(())
        });
        if matches!(&result, Err(jni::errors::Error::JavaException)) {
            // Convert the exception into a native error while this thread is still attached.
            let _ = env.exception_clear();
        }
        result.map_err(|error| io::Error::other(format!("Android document {method}: {error}")))
    }

    fn close(&mut self, failed: bool) -> io::Result<()> {
        let Some(object) = self.object.take() else {
            return Ok(());
        };
        if failed {
            self.call(
                &object,
                "closeWithError",
                Some("MJSol'Ex could not write the image"),
            )
        } else {
            self.call(&object, "close", None)
        }
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // Only finish_export can commit a destination. An abandoned picker event
        // or failed worker launch must tell a reliable provider the write failed.
        let _ = self.close(self.export);
    }
}
