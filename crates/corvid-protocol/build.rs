use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use prost_types::FileDescriptorSet;
use prost_types::field_descriptor_proto::{Label, Type};

const PROTO: &str = "proto/corvid.proto";
const PACKAGE: &str = "corvid.protocol.v1";
const REFERENCE_PROTOC: &str = "CORVID_SCHEMA_REFERENCE_PROTOC";
const PROTOC_VERSION: &str = "libprotoc 36.1";

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed={PROTO}");
    println!("cargo:rerun-if-env-changed={REFERENCE_PROTOC}");

    let mut config = prost_build::Config::new();
    config.boxed(format!(".{PACKAGE}.SnapshotEntity.body.agent"));
    let descriptors = match env::var_os(REFERENCE_PROTOC) {
        Some(protoc) => reference_descriptors(&mut config, protoc.into())?,
        None => protox::compile([PROTO], ["proto"])?,
    };
    let preflight = preflight_tables(&descriptors)?;
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?);
    fs::write(out_dir.join("wire_preflight.rs"), preflight)?;
    config.compile_fds(descriptors)?;
    Ok(())
}

fn reference_descriptors(
    config: &mut prost_build::Config,
    protoc: PathBuf,
) -> Result<FileDescriptorSet, Box<dyn Error>> {
    let version = Command::new(&protoc)
        .arg("--version")
        .output()
        .map_err(|error| {
            format!(
                "could not run reference compiler {} ({error})",
                protoc.display()
            )
        })?;
    let version_output = String::from_utf8_lossy(&version.stdout);
    if !version.status.success() || version_output.trim() != PROTOC_VERSION {
        return Err(format!(
            "{} --version returned {} with stdout {:?}. Corvid requires {PROTOC_VERSION} to build the wire schema.",
            protoc.display(),
            version.status,
            version_output.trim()
        )
        .into());
    }

    config.protoc_executable(protoc);
    Ok(config.load_fds(&[PROTO], &["proto"])?)
}

fn preflight_tables(descriptors: &FileDescriptorSet) -> Result<String, Box<dyn Error>> {
    let file = descriptors
        .file
        .iter()
        .find(|file| file.package() == PACKAGE)
        .ok_or("missing Corvid schema package")?;
    let messages = &file.message_type;
    let mut output = String::from("const SHAPES: &[&[Field]] = &[\n");
    for message in messages {
        if !message.nested_type.is_empty() {
            return Err("wire messages must use top-level declarations".into());
        }
        writeln!(output, "    &[")?;
        for field in &message.field {
            let child = if field.r#type() == Type::Message {
                let qualified_name = field.type_name();
                let index = messages
                    .iter()
                    .position(|message| qualified_name == format!(".{PACKAGE}.{}", message.name()))
                    .ok_or("unresolved wire message type")?;
                Some(index)
            } else {
                None
            };
            let repeated = field.label() == Label::Repeated;
            let packed_varints = repeated
                && matches!(
                    field.r#type(),
                    Type::Bool
                        | Type::Enum
                        | Type::Int32
                        | Type::Int64
                        | Type::Sint32
                        | Type::Sint64
                        | Type::Uint32
                        | Type::Uint64
                );
            if repeated || child.is_some() {
                writeln!(
                    output,
                    "        Field {{ tag: {}, child: {child:?}, repeated: {repeated}, packed_varints: {packed_varints} }},",
                    field.number()
                )?;
            }
        }
        writeln!(output, "    ],")?;
    }
    writeln!(output, "];")?;

    for (constant, message_name) in [("ENVELOPE", "Envelope"), ("RENDER_HEADER", "RenderHeader")] {
        let index = messages
            .iter()
            .position(|message| message.name() == message_name)
            .ok_or("missing root wire type")?;
        writeln!(output, "const {constant}: usize = {index};")?;
    }
    Ok(output)
}
