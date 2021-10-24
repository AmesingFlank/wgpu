use super::{
    helpers::{contains_builtin, map_storage_class},
    make_local, Block, BlockContext, CachedExpressions, EntryPointContext, Error, Function,
    FunctionArgument, GlobalVariable, IdGenerator, Instruction, LocalType, LocalVariable,
    LogicalLayout, LookupFunctionType, LookupType, LoopContext, Options, PhysicalLayout,
    PipelineOptions, ResultMember, Writer, WriterFlags, BITS_PER_BYTE,
};
use crate::{
    arena::{Handle, UniqueArena},
    proc::TypeResolution,
    valid::{FunctionInfo, ModuleInfo},
};
use spirv::Word;
use std::collections::hash_map::Entry;

struct FunctionInterface<'a> {
    varying_ids: &'a mut Vec<Word>,
    stage: crate::ShaderStage,
}

impl Function {
    fn to_words(&self, sink: &mut impl Extend<Word>) {
        self.signature.as_ref().unwrap().to_words(sink);
        for argument in self.parameters.iter() {
            argument.instruction.to_words(sink);
        }
        for (index, block) in self.blocks.iter().enumerate() {
            Instruction::label(block.label_id).to_words(sink);
            if index == 0 {
                for local_var in self.variables.values() {
                    local_var.instruction.to_words(sink);
                }
            }
            for instruction in block.body.iter() {
                instruction.to_words(sink);
            }
        }
    }
}

impl Writer {
    pub fn new(options: &Options) -> Result<Self, Error> {
        let (major, minor) = options.lang_version;
        if major != 1 {
            return Err(Error::UnsupportedVersion(major, minor));
        }
        let raw_version = ((major as u32) << 16) | ((minor as u32) << 8);

        let mut capabilities_used = crate::FastHashSet::default();
        capabilities_used.insert(spirv::Capability::Shader);

        let mut id_gen = IdGenerator::default();
        let gl450_ext_inst_id = id_gen.next();
        let void_type = id_gen.next();

        Ok(Writer {
            physical_layout: PhysicalLayout::new(raw_version),
            logical_layout: LogicalLayout::default(),
            id_gen,
            capabilities_available: options.capabilities.clone(),
            capabilities_used,
            debugs: vec![],
            annotations: vec![],
            flags: options.flags,
            bounds_check_policies: options.bounds_check_policies,
            void_type,
            lookup_type: crate::FastHashMap::default(),
            lookup_function: crate::FastHashMap::default(),
            lookup_function_type: crate::FastHashMap::default(),
            constant_ids: Vec::new(),
            cached_constants: crate::FastHashMap::default(),
            global_variables: Vec::new(),
            saved_cached: CachedExpressions::default(),
            gl450_ext_inst_id,
            temp_list: Vec::new(),
        })
    }

    /// Reset `Writer` to its initial state, retaining any allocations.
    ///
    /// Why not just implement `Recyclable` for `Writer`? By design,
    /// `Recyclable::recycle` requires ownership of the value, not just
    /// `&mut`; see the trait documentation. But we need to use this method
    /// from functions like `Writer::write`, which only have `&mut Writer`.
    /// Workarounds include unsafe code (`std::ptr::read`, then `write`, ugh)
    /// or something like a `Default` impl that returns an oddly-initialized
    /// `Writer`, which is worse.
    fn reset(&mut self) {
        use super::recyclable::Recyclable;
        use std::mem::take;

        let mut id_gen = IdGenerator::default();
        let gl450_ext_inst_id = id_gen.next();
        let void_type = id_gen.next();

        // Every field of the old writer that is not determined by the `Options`
        // passed to `Writer::new` should be reset somehow.
        let fresh = Writer {
            // Copied from the old Writer:
            flags: self.flags,
            bounds_check_policies: self.bounds_check_policies,
            capabilities_available: take(&mut self.capabilities_available),

            // Initialized afresh:
            id_gen,
            void_type,
            gl450_ext_inst_id,

            // Recycled:
            capabilities_used: take(&mut self.capabilities_used).recycle(),
            physical_layout: self.physical_layout.clone().recycle(),
            logical_layout: take(&mut self.logical_layout).recycle(),
            debugs: take(&mut self.debugs).recycle(),
            annotations: take(&mut self.annotations).recycle(),
            lookup_type: take(&mut self.lookup_type).recycle(),
            lookup_function: take(&mut self.lookup_function).recycle(),
            lookup_function_type: take(&mut self.lookup_function_type).recycle(),
            constant_ids: take(&mut self.constant_ids).recycle(),
            cached_constants: take(&mut self.cached_constants).recycle(),
            global_variables: take(&mut self.global_variables).recycle(),
            saved_cached: take(&mut self.saved_cached).recycle(),
            temp_list: take(&mut self.temp_list).recycle(),
        };

        *self = fresh;

        self.capabilities_used.insert(spirv::Capability::Shader);
    }

    /// Indicate that the code requires any one of the listed capabilities.
    ///
    /// If nothing in `capabilities` appears in the available capabilities
    /// specified in the [`Options`] from which this `Writer` was created,
    /// return an error. The `what` string is used in the error message to
    /// explain what provoked the requirement. (If no available capabilites were
    /// given, assume everything is available.)
    ///
    /// The first acceptable capability will be added to this `Writer`'s
    /// [`capabilities_used`] table, and an `OpCapability` emitted for it in the
    /// result. For this reason, more specific capabilities should be listed
    /// before more general.
    ///
    /// [`capabilities_used`]: Writer::capabilities_used
    pub(super) fn require_any(
        &mut self,
        what: &'static str,
        capabilities: &[spirv::Capability],
    ) -> Result<(), Error> {
        match *capabilities {
            [] => Ok(()),
            [first, ..] => {
                // Find the first acceptable capability, or return an error if
                // there is none.
                let selected = match self.capabilities_available {
                    None => first,
                    Some(ref available) => {
                        match capabilities.iter().find(|cap| available.contains(cap)) {
                            Some(&cap) => cap,
                            None => {
                                return Err(Error::MissingCapabilities(what, capabilities.to_vec()))
                            }
                        }
                    }
                };
                self.capabilities_used.insert(selected);
                Ok(())
            }
        }
    }

    pub(super) fn get_type_id(&mut self, lookup_ty: LookupType) -> Word {
        match self.lookup_type.entry(lookup_ty) {
            Entry::Occupied(e) => *e.get(),
            Entry::Vacant(e) => {
                let local = match lookup_ty {
                    LookupType::Handle(_handle) => unreachable!("Handles are populated at start"),
                    LookupType::Local(local) => local,
                };

                let id = self.id_gen.next();
                e.insert(id);
                self.write_type_declaration_local(id, local);
                id
            }
        }
    }

    pub(super) fn get_expression_type_id(&mut self, tr: &TypeResolution) -> Word {
        let lookup_ty = match *tr {
            TypeResolution::Handle(ty_handle) => LookupType::Handle(ty_handle),
            TypeResolution::Value(ref inner) => LookupType::Local(make_local(inner).unwrap()),
        };
        self.get_type_id(lookup_ty)
    }

    pub(super) fn get_pointer_id(
        &mut self,
        arena: &UniqueArena<crate::Type>,
        handle: Handle<crate::Type>,
        class: spirv::StorageClass,
    ) -> Result<Word, Error> {
        let ty_id = self.get_type_id(LookupType::Handle(handle));
        if let crate::TypeInner::Pointer { .. } = arena[handle].inner {
            return Ok(ty_id);
        }
        let lookup_type = LookupType::Local(LocalType::Pointer {
            base: handle,
            class,
        });
        Ok(if let Some(&id) = self.lookup_type.get(&lookup_type) {
            id
        } else {
            let id = self.id_gen.next();
            let instruction = Instruction::type_pointer(id, class, ty_id);
            instruction.to_words(&mut self.logical_layout.declarations);
            self.lookup_type.insert(lookup_type, id);
            id
        })
    }

    pub(super) fn get_uint_type_id(&mut self) -> Word {
        let local_type = LocalType::Value {
            vector_size: None,
            kind: crate::ScalarKind::Uint,
            width: 4,
            pointer_class: None,
        };
        self.get_type_id(local_type.into())
    }

    pub(super) fn get_float_type_id(&mut self) -> Word {
        let local_type = LocalType::Value {
            vector_size: None,
            kind: crate::ScalarKind::Float,
            width: 4,
            pointer_class: None,
        };
        self.get_type_id(local_type.into())
    }

    pub(super) fn get_float_pointer_type_id(&mut self, class: spirv::StorageClass) -> Word {
        let lookup_type = LookupType::Local(LocalType::Value {
            vector_size: None,
            kind: crate::ScalarKind::Float,
            width: 4,
            pointer_class: Some(class),
        });
        if let Some(&id) = self.lookup_type.get(&lookup_type) {
            id
        } else {
            let id = self.id_gen.next();
            let ty_id = self.get_float_type_id();
            let instruction = Instruction::type_pointer(id, class, ty_id);
            instruction.to_words(&mut self.logical_layout.declarations);
            self.lookup_type.insert(lookup_type, id);
            id
        }
    }

    pub(super) fn get_bool_type_id(&mut self) -> Word {
        let local_type = LocalType::Value {
            vector_size: None,
            kind: crate::ScalarKind::Bool,
            width: 1,
            pointer_class: None,
        };
        self.get_type_id(local_type.into())
    }

    fn decorate(&mut self, id: Word, decoration: spirv::Decoration, operands: &[Word]) {
        self.annotations
            .push(Instruction::decorate(id, decoration, operands));
    }

    fn write_function(
        &mut self,
        ir_function: &crate::Function,
        info: &FunctionInfo,
        ir_module: &crate::Module,
        mut interface: Option<FunctionInterface>,
    ) -> Result<Word, Error> {
        let mut function = Function::default();

        for (handle, variable) in ir_function.local_variables.iter() {
            let id = self.id_gen.next();

            if self.flags.contains(WriterFlags::DEBUG) {
                if let Some(ref name) = variable.name {
                    self.debugs.push(Instruction::name(id, name));
                }
            }

            let init_word = variable
                .init
                .map(|constant| self.constant_ids[constant.index()]);
            let pointer_type_id =
                self.get_pointer_id(&ir_module.types, variable.ty, spirv::StorageClass::Function)?;
            let instruction = Instruction::variable(
                pointer_type_id,
                id,
                spirv::StorageClass::Function,
                init_word,
            );
            function
                .variables
                .insert(handle, LocalVariable { id, instruction });
        }

        let prelude_id = self.id_gen.next();
        let mut prelude = Block::new(prelude_id);
        let mut ep_context = EntryPointContext {
            argument_ids: Vec::new(),
            results: Vec::new(),
        };

        let mut parameter_type_ids = Vec::with_capacity(ir_function.arguments.len());
        for argument in ir_function.arguments.iter() {
            let class = spirv::StorageClass::Input;
            let handle_ty = ir_module.types[argument.ty].inner.is_handle();
            let argument_type_id = match handle_ty {
                true => self.get_pointer_id(
                    &ir_module.types,
                    argument.ty,
                    spirv::StorageClass::UniformConstant,
                )?,
                false => self.get_type_id(LookupType::Handle(argument.ty)),
            };

            if let Some(ref mut iface) = interface {
                let id = if let Some(ref binding) = argument.binding {
                    let name = argument.name.as_ref().map(AsRef::as_ref);
                    let varying_id =
                        self.write_varying(ir_module, class, name, argument.ty, binding)?;
                    iface.varying_ids.push(varying_id);
                    let id = self.id_gen.next();
                    prelude
                        .body
                        .push(Instruction::load(argument_type_id, id, varying_id, None));
                    id
                } else if let crate::TypeInner::Struct { ref members, .. } =
                    ir_module.types[argument.ty].inner
                {
                    let struct_id = self.id_gen.next();
                    let mut constituent_ids = Vec::with_capacity(members.len());
                    for member in members {
                        let type_id = self.get_type_id(LookupType::Handle(member.ty));
                        let name = member.name.as_ref().map(AsRef::as_ref);
                        let binding = member.binding.as_ref().unwrap();
                        let varying_id =
                            self.write_varying(ir_module, class, name, member.ty, binding)?;
                        iface.varying_ids.push(varying_id);
                        let id = self.id_gen.next();
                        prelude
                            .body
                            .push(Instruction::load(type_id, id, varying_id, None));
                        constituent_ids.push(id);
                    }
                    prelude.body.push(Instruction::composite_construct(
                        argument_type_id,
                        struct_id,
                        &constituent_ids,
                    ));
                    struct_id
                } else {
                    unreachable!("Missing argument binding on an entry point");
                };
                ep_context.argument_ids.push(id);
            } else {
                let argument_id = self.id_gen.next();
                let instruction = Instruction::function_parameter(argument_type_id, argument_id);
                if self.flags.contains(WriterFlags::DEBUG) {
                    if let Some(ref name) = argument.name {
                        self.debugs.push(Instruction::name(argument_id, name));
                    }
                }
                function.parameters.push(FunctionArgument {
                    instruction,
                    handle_id: if handle_ty {
                        let id = self.id_gen.next();
                        prelude.body.push(Instruction::load(
                            self.get_type_id(LookupType::Handle(argument.ty)),
                            id,
                            argument_id,
                            None,
                        ));
                        id
                    } else {
                        0
                    },
                });
                parameter_type_ids.push(argument_type_id);
            };
        }

        let return_type_id = match ir_function.result {
            Some(ref result) => {
                if let Some(ref mut iface) = interface {
                    let mut has_point_size = false;
                    let class = spirv::StorageClass::Output;
                    if let Some(ref binding) = result.binding {
                        has_point_size |=
                            *binding == crate::Binding::BuiltIn(crate::BuiltIn::PointSize);
                        let type_id = self.get_type_id(LookupType::Handle(result.ty));
                        let varying_id =
                            self.write_varying(ir_module, class, None, result.ty, binding)?;
                        iface.varying_ids.push(varying_id);
                        ep_context.results.push(ResultMember {
                            id: varying_id,
                            type_id,
                            built_in: binding.to_built_in(),
                        });
                    } else if let crate::TypeInner::Struct { ref members, .. } =
                        ir_module.types[result.ty].inner
                    {
                        for member in members {
                            let type_id = self.get_type_id(LookupType::Handle(member.ty));
                            let name = member.name.as_ref().map(AsRef::as_ref);
                            let binding = member.binding.as_ref().unwrap();
                            has_point_size |=
                                *binding == crate::Binding::BuiltIn(crate::BuiltIn::PointSize);
                            let varying_id =
                                self.write_varying(ir_module, class, name, member.ty, binding)?;
                            iface.varying_ids.push(varying_id);
                            ep_context.results.push(ResultMember {
                                id: varying_id,
                                type_id,
                                built_in: binding.to_built_in(),
                            });
                        }
                    } else {
                        unreachable!("Missing result binding on an entry point");
                    }

                    if self.flags.contains(WriterFlags::FORCE_POINT_SIZE)
                        && iface.stage == crate::ShaderStage::Vertex
                        && !has_point_size
                    {
                        // add point size artificially
                        let varying_id = self.id_gen.next();
                        let pointer_type_id = self.get_float_pointer_type_id(class);
                        Instruction::variable(pointer_type_id, varying_id, class, None)
                            .to_words(&mut self.logical_layout.declarations);
                        self.decorate(
                            varying_id,
                            spirv::Decoration::BuiltIn,
                            &[spirv::BuiltIn::PointSize as u32],
                        );
                        iface.varying_ids.push(varying_id);

                        let default_value_id =
                            self.get_constant_scalar(crate::ScalarValue::Float(1.0), 4);
                        prelude
                            .body
                            .push(Instruction::store(varying_id, default_value_id, None));
                    }
                    self.void_type
                } else {
                    self.get_type_id(LookupType::Handle(result.ty))
                }
            }
            None => self.void_type,
        };

        let lookup_function_type = LookupFunctionType {
            parameter_type_ids,
            return_type_id,
        };

        let function_id = self.id_gen.next();
        if self.flags.contains(WriterFlags::DEBUG) {
            if let Some(ref name) = ir_function.name {
                self.debugs.push(Instruction::name(function_id, name));
            }
        }

        let function_type = self.get_function_type(lookup_function_type);
        function.signature = Some(Instruction::function(
            return_type_id,
            function_id,
            spirv::FunctionControl::empty(),
            function_type,
        ));

        if interface.is_some() {
            function.entry_point_context = Some(ep_context);
        }

        // fill up the `GlobalVariable::handle_id`
        for gv in self.global_variables.iter_mut() {
            gv.reset_for_function();
        }
        for (handle, var) in ir_module.global_variables.iter() {
            // Handle globals are pre-emitted and should be loaded automatically.
            if info[handle].is_empty() || var.class != crate::StorageClass::Handle {
                continue;
            }
            let id = self.id_gen.next();
            let result_type_id = self.get_type_id(LookupType::Handle(var.ty));
            let gv = &mut self.global_variables[handle.index()];
            prelude
                .body
                .push(Instruction::load(result_type_id, id, gv.id, None));
            gv.handle_id = id;
        }

        // Create a `BlockContext` for generating SPIR-V for the function's
        // body.
        let mut context = BlockContext {
            ir_module,
            ir_function,
            fun_info: info,
            function: &mut function,
            // Re-use the cached expression table from prior functions.
            cached: std::mem::take(&mut self.saved_cached),

            // Steal the Writer's temp list for a bit.
            temp_list: std::mem::take(&mut self.temp_list),
            writer: self,
        };

        // fill up the pre-emitted expressions
        context.cached.reset(ir_function.expressions.len());
        for (handle, expr) in ir_function.expressions.iter() {
            if expr.needs_pre_emit() {
                context.cache_expression_value(handle, &mut prelude)?;
            }
        }

        let main_id = context.gen_id();
        context
            .function
            .consume(prelude, Instruction::branch(main_id));
        context.write_block(main_id, &ir_function.body, None, LoopContext::default())?;

        // Consume the `BlockContext`, ending its borrows and letting the
        // `Writer` steal back its cached expression table and temp_list.
        let BlockContext {
            cached, temp_list, ..
        } = context;
        self.saved_cached = cached;
        self.temp_list = temp_list;

        function.to_words(&mut self.logical_layout.function_definitions);
        Instruction::function_end().to_words(&mut self.logical_layout.function_definitions);

        Ok(function_id)
    }

    fn write_execution_mode(
        &mut self,
        function_id: Word,
        mode: spirv::ExecutionMode,
    ) -> Result<(), Error> {
        //self.check(mode.required_capabilities())?;
        Instruction::execution_mode(function_id, mode, &[])
            .to_words(&mut self.logical_layout.execution_modes);
        Ok(())
    }

    // TODO Move to instructions module
    fn write_entry_point(
        &mut self,
        entry_point: &crate::EntryPoint,
        info: &FunctionInfo,
        ir_module: &crate::Module,
    ) -> Result<Instruction, Error> {
        let mut interface_ids = Vec::new();
        let function_id = self.write_function(
            &entry_point.function,
            info,
            ir_module,
            Some(FunctionInterface {
                varying_ids: &mut interface_ids,
                stage: entry_point.stage,
            }),
        )?;

        let exec_model = match entry_point.stage {
            crate::ShaderStage::Vertex => spirv::ExecutionModel::Vertex,
            crate::ShaderStage::Fragment => {
                self.write_execution_mode(function_id, spirv::ExecutionMode::OriginUpperLeft)?;
                if let Some(ref result) = entry_point.function.result {
                    if contains_builtin(
                        result.binding.as_ref(),
                        result.ty,
                        &ir_module.types,
                        crate::BuiltIn::FragDepth,
                    ) {
                        self.write_execution_mode(
                            function_id,
                            spirv::ExecutionMode::DepthReplacing,
                        )?;
                    }
                }
                spirv::ExecutionModel::Fragment
            }
            crate::ShaderStage::Compute => {
                let execution_mode = spirv::ExecutionMode::LocalSize;
                //self.check(execution_mode.required_capabilities())?;
                Instruction::execution_mode(
                    function_id,
                    execution_mode,
                    &entry_point.workgroup_size,
                )
                .to_words(&mut self.logical_layout.execution_modes);
                spirv::ExecutionModel::GLCompute
            }
        };
        //self.check(exec_model.required_capabilities())?;

        Ok(Instruction::entry_point(
            exec_model,
            function_id,
            &entry_point.name,
            interface_ids.as_slice(),
        ))
    }

    fn make_scalar(
        &mut self,
        id: Word,
        kind: crate::ScalarKind,
        width: crate::Bytes,
    ) -> Instruction {
        use crate::ScalarKind as Sk;

        let bits = (width * BITS_PER_BYTE) as u32;
        match kind {
            Sk::Sint | Sk::Uint => {
                let signedness = if kind == Sk::Sint {
                    super::instructions::Signedness::Signed
                } else {
                    super::instructions::Signedness::Unsigned
                };
                let cap = match bits {
                    8 => Some(spirv::Capability::Int8),
                    16 => Some(spirv::Capability::Int16),
                    64 => Some(spirv::Capability::Int64),
                    _ => None,
                };
                if let Some(cap) = cap {
                    self.capabilities_used.insert(cap);
                }
                Instruction::type_int(id, bits, signedness)
            }
            Sk::Float => {
                if bits == 64 {
                    self.capabilities_used.insert(spirv::Capability::Float64);
                }
                Instruction::type_float(id, bits)
            }
            Sk::Bool => Instruction::type_bool(id),
        }
    }

    fn request_image_capabilities(&mut self, inner: &crate::TypeInner) -> Result<(), Error> {
        if let crate::TypeInner::Image {
            dim,
            arrayed,
            class,
        } = *inner
        {
            let sampled = match class {
                crate::ImageClass::Sampled { .. } => true,
                crate::ImageClass::Depth { .. } => true,
                crate::ImageClass::Storage { format, .. } => {
                    self.request_image_format_capabilities(format.into())?;
                    false
                }
            };

            match dim {
                crate::ImageDimension::D1 => {
                    if sampled {
                        self.require_any("sampled 1D images", &[spirv::Capability::Sampled1D])?;
                    } else {
                        self.require_any("1D storage images", &[spirv::Capability::Image1D])?;
                    }
                }
                crate::ImageDimension::Cube if arrayed => {
                    if sampled {
                        self.require_any(
                            "sampled cube array images",
                            &[spirv::Capability::SampledCubeArray],
                        )?;
                    } else {
                        self.require_any(
                            "cube array storage images",
                            &[spirv::Capability::ImageCubeArray],
                        )?;
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn write_type_declaration_local(&mut self, id: Word, local_ty: LocalType) {
        let instruction = match local_ty {
            LocalType::Value {
                vector_size: None,
                kind,
                width,
                pointer_class: None,
            } => self.make_scalar(id, kind, width),
            LocalType::Value {
                vector_size: Some(size),
                kind,
                width,
                pointer_class: None,
            } => {
                let scalar_id = self.get_type_id(LookupType::Local(LocalType::Value {
                    vector_size: None,
                    kind,
                    width,
                    pointer_class: None,
                }));
                Instruction::type_vector(id, scalar_id, size)
            }
            LocalType::Matrix {
                columns,
                rows,
                width,
            } => {
                let vector_id = self.get_type_id(LookupType::Local(LocalType::Value {
                    vector_size: Some(rows),
                    kind: crate::ScalarKind::Float,
                    width,
                    pointer_class: None,
                }));
                Instruction::type_matrix(id, vector_id, columns)
            }
            LocalType::Pointer { base, class } => {
                let type_id = self.get_type_id(LookupType::Handle(base));
                Instruction::type_pointer(id, class, type_id)
            }
            LocalType::Value {
                vector_size,
                kind,
                width,
                pointer_class: Some(class),
            } => {
                let type_id = self.get_type_id(LookupType::Local(LocalType::Value {
                    vector_size,
                    kind,
                    width,
                    pointer_class: None,
                }));
                Instruction::type_pointer(id, class, type_id)
            }
            LocalType::Image(image) => {
                let local_type = LocalType::Value {
                    vector_size: None,
                    kind: image.sampled_type,
                    width: 4,
                    pointer_class: None,
                };
                let type_id = self.get_type_id(LookupType::Local(local_type));
                Instruction::type_image(id, type_id, image.dim, image.flags, image.image_format)
            }
            LocalType::Sampler => Instruction::type_sampler(id),
            LocalType::SampledImage { image_type_id } => {
                Instruction::type_sampled_image(id, image_type_id)
            }
        };

        instruction.to_words(&mut self.logical_layout.declarations);
    }

    fn write_type_declaration_arena(
        &mut self,
        arena: &UniqueArena<crate::Type>,
        handle: Handle<crate::Type>,
    ) -> Result<Word, Error> {
        let ty = &arena[handle];
        let decorate_layout = true; //TODO?

        let id = if let Some(local) = make_local(&ty.inner) {
            // This type can be represented as a `LocalType`, so check if we've
            // already written an instruction for it. If not, do so now, with
            // `write_type_declaration_local`.
            match self.lookup_type.entry(LookupType::Local(local)) {
                // We already have an id for this `LocalType`.
                Entry::Occupied(e) => *e.get(),

                // It's a type we haven't seen before.
                Entry::Vacant(e) => {
                    let id = self.id_gen.next();
                    e.insert(id);
                    self.write_type_declaration_local(id, local);

                    // If it's an image type, request SPIR-V capabilities here, so
                    // write_type_declaration_local can stay infallible.
                    self.request_image_capabilities(&ty.inner)?;

                    id
                }
            }
        } else {
            use spirv::Decoration;

            let id = self.id_gen.next();
            let instruction = match ty.inner {
                crate::TypeInner::Array { base, size, stride } => {
                    if decorate_layout {
                        self.decorate(id, Decoration::ArrayStride, &[stride]);
                    }

                    let type_id = self.get_type_id(LookupType::Handle(base));
                    match size {
                        crate::ArraySize::Constant(const_handle) => {
                            let length_id = self.constant_ids[const_handle.index()];
                            Instruction::type_array(id, type_id, length_id)
                        }
                        crate::ArraySize::Dynamic => Instruction::type_runtime_array(id, type_id),
                    }
                }
                crate::TypeInner::Struct {
                    top_level,
                    ref members,
                    span: _,
                } => {
                    if top_level {
                        self.decorate(id, Decoration::Block, &[]);
                    }

                    let mut member_ids = Vec::with_capacity(members.len());
                    for (index, member) in members.iter().enumerate() {
                        if decorate_layout {
                            self.annotations.push(Instruction::member_decorate(
                                id,
                                index as u32,
                                Decoration::Offset,
                                &[member.offset],
                            ));
                        }

                        if self.flags.contains(WriterFlags::DEBUG) {
                            if let Some(ref name) = member.name {
                                self.debugs
                                    .push(Instruction::member_name(id, index as u32, name));
                            }
                        }

                        // The matrix decorations also go on arrays of matrices,
                        // so lets check this first.
                        let member_array_subty_inner = match arena[member.ty].inner {
                            crate::TypeInner::Array { base, .. } => &arena[base].inner,
                            ref other => other,
                        };
                        if let crate::TypeInner::Matrix {
                            columns,
                            rows: _,
                            width,
                        } = *member_array_subty_inner
                        {
                            let byte_stride = match columns {
                                crate::VectorSize::Bi => 2 * width,
                                crate::VectorSize::Tri | crate::VectorSize::Quad => 4 * width,
                            };
                            self.annotations.push(Instruction::member_decorate(
                                id,
                                index as u32,
                                Decoration::ColMajor,
                                &[],
                            ));
                            self.annotations.push(Instruction::member_decorate(
                                id,
                                index as u32,
                                Decoration::MatrixStride,
                                &[byte_stride as u32],
                            ));
                        }

                        let member_id = self.get_type_id(LookupType::Handle(member.ty));
                        member_ids.push(member_id);
                    }
                    Instruction::type_struct(id, member_ids.as_slice())
                }

                // These all have TypeLocal representations, so they should have been
                // handled by `write_type_declaration_local` above.
                crate::TypeInner::Scalar { .. }
                | crate::TypeInner::Atomic { .. }
                | crate::TypeInner::Vector { .. }
                | crate::TypeInner::Matrix { .. }
                | crate::TypeInner::Pointer { .. }
                | crate::TypeInner::ValuePointer { .. }
                | crate::TypeInner::Image { .. }
                | crate::TypeInner::Sampler { .. } => unreachable!(),
            };

            instruction.to_words(&mut self.logical_layout.declarations);
            id
        };

        // Add this handle as a new alias for that type.
        self.lookup_type.insert(LookupType::Handle(handle), id);

        if self.flags.contains(WriterFlags::DEBUG) {
            if let Some(ref name) = ty.name {
                self.debugs.push(Instruction::name(id, name));
            }
        }

        Ok(id)
    }

    fn request_image_format_capabilities(
        &mut self,
        format: spirv::ImageFormat,
    ) -> Result<(), Error> {
        use spirv::ImageFormat as If;
        match format {
            If::Rg32f
            | If::Rg16f
            | If::R11fG11fB10f
            | If::R16f
            | If::Rgba16
            | If::Rgb10A2
            | If::Rg16
            | If::Rg8
            | If::R16
            | If::R8
            | If::Rgba16Snorm
            | If::Rg16Snorm
            | If::Rg8Snorm
            | If::R16Snorm
            | If::R8Snorm
            | If::Rg32i
            | If::Rg16i
            | If::Rg8i
            | If::R16i
            | If::R8i
            | If::Rgb10a2ui
            | If::Rg32ui
            | If::Rg16ui
            | If::Rg8ui
            | If::R16ui
            | If::R8ui => self.require_any(
                "storage image format",
                &[spirv::Capability::StorageImageExtendedFormats],
            ),
            If::R64ui | If::R64i => self.require_any(
                "64-bit integer storage image format",
                &[spirv::Capability::Int64ImageEXT],
            ),
            If::Unknown
            | If::Rgba32f
            | If::Rgba16f
            | If::R32f
            | If::Rgba8
            | If::Rgba8Snorm
            | If::Rgba32i
            | If::Rgba16i
            | If::Rgba8i
            | If::R32i
            | If::Rgba32ui
            | If::Rgba16ui
            | If::Rgba8ui
            | If::R32ui => Ok(()),
        }
    }

    pub(super) fn get_index_constant(&mut self, index: Word) -> Word {
        self.get_constant_scalar(crate::ScalarValue::Uint(index as _), 4)
    }

    pub(super) fn get_constant_scalar(
        &mut self,
        value: crate::ScalarValue,
        width: crate::Bytes,
    ) -> Word {
        if let Some(&id) = self.cached_constants.get(&(value, width)) {
            return id;
        }
        let id = self.id_gen.next();
        self.write_constant_scalar(id, &value, width, None);
        self.cached_constants.insert((value, width), id);
        id
    }

    fn write_constant_scalar(
        &mut self,
        id: Word,
        value: &crate::ScalarValue,
        width: crate::Bytes,
        debug_name: Option<&String>,
    ) {
        if self.flags.contains(WriterFlags::DEBUG) {
            if let Some(name) = debug_name {
                self.debugs.push(Instruction::name(id, name));
            }
        }
        let type_id = self.get_type_id(LookupType::Local(LocalType::Value {
            vector_size: None,
            kind: value.scalar_kind(),
            width,
            pointer_class: None,
        }));
        let (solo, pair);
        let instruction = match *value {
            crate::ScalarValue::Sint(val) => {
                let words = match width {
                    4 => {
                        solo = [val as u32];
                        &solo[..]
                    }
                    8 => {
                        pair = [(val >> 32) as u32, val as u32];
                        &pair
                    }
                    _ => unreachable!(),
                };
                Instruction::constant(type_id, id, words)
            }
            crate::ScalarValue::Uint(val) => {
                let words = match width {
                    4 => {
                        solo = [val as u32];
                        &solo[..]
                    }
                    8 => {
                        pair = [(val >> 32) as u32, val as u32];
                        &pair
                    }
                    _ => unreachable!(),
                };
                Instruction::constant(type_id, id, words)
            }
            crate::ScalarValue::Float(val) => {
                let words = match width {
                    4 => {
                        solo = [(val as f32).to_bits()];
                        &solo[..]
                    }
                    8 => {
                        let bits = f64::to_bits(val);
                        pair = [(bits >> 32) as u32, bits as u32];
                        &pair
                    }
                    _ => unreachable!(),
                };
                Instruction::constant(type_id, id, words)
            }
            crate::ScalarValue::Bool(true) => Instruction::constant_true(type_id, id),
            crate::ScalarValue::Bool(false) => Instruction::constant_false(type_id, id),
        };

        instruction.to_words(&mut self.logical_layout.declarations);
    }

    fn write_constant_composite(
        &mut self,
        id: Word,
        ty: Handle<crate::Type>,
        components: &[Handle<crate::Constant>],
    ) -> Result<(), Error> {
        let mut constituent_ids = Vec::with_capacity(components.len());
        for constituent in components.iter() {
            let constituent_id = self.constant_ids[constituent.index()];
            constituent_ids.push(constituent_id);
        }

        let type_id = self.get_type_id(LookupType::Handle(ty));
        Instruction::constant_composite(type_id, id, constituent_ids.as_slice())
            .to_words(&mut self.logical_layout.declarations);
        Ok(())
    }

    pub(super) fn write_constant_null(&mut self, type_id: Word) -> Word {
        let null_id = self.id_gen.next();
        Instruction::constant_null(type_id, null_id)
            .to_words(&mut self.logical_layout.declarations);
        null_id
    }

    /// Generate an `OpVariable` for one value in an [`EntryPoint`]'s IO interface.
    ///
    /// The [`Binding`]s of the arguments and result of an [`EntryPoint`]'s
    /// [`Function`] describe a SPIR-V shader interface. In SPIR-V, the
    /// interface is represented by global variables in the `Input` and `Output`
    /// storage classes, with decorations indicating which builtin or location
    /// each variable corresponds to.
    ///
    /// This function emits a single global `OpVariable` for a single value from
    /// the interface, and adds appropriate decorations to indicate which
    /// builtin or location it represents, how it should be interpolated, and so
    /// on. The `class` argument gives the variable's SPIR-V storage class,
    /// which should be either [`Input`] or [`Output`].
    ///
    /// [`Binding`]: crate::Binding
    /// [`Function`]: crate::Function
    /// [`EntryPoint`]: crate::EntryPoint
    /// [`Input`]: spirv::StorageClass::Input
    /// [`Output`]: spirv::StorageClass::Output
    fn write_varying(
        &mut self,
        ir_module: &crate::Module,
        class: spirv::StorageClass,
        debug_name: Option<&str>,
        ty: Handle<crate::Type>,
        binding: &crate::Binding,
    ) -> Result<Word, Error> {
        let id = self.id_gen.next();
        let pointer_type_id = self.get_pointer_id(&ir_module.types, ty, class)?;
        Instruction::variable(pointer_type_id, id, class, None)
            .to_words(&mut self.logical_layout.declarations);

        if self
            .flags
            .contains(WriterFlags::DEBUG | WriterFlags::LABEL_VARYINGS)
        {
            if let Some(name) = debug_name {
                self.debugs.push(Instruction::name(id, name));
            }
        }

        use spirv::{BuiltIn, Decoration};

        match *binding {
            crate::Binding::Location {
                location,
                interpolation,
                sampling,
            } => {
                self.decorate(id, Decoration::Location, &[location]);

                match interpolation {
                    // Perspective-correct interpolation is the default in SPIR-V.
                    None | Some(crate::Interpolation::Perspective) => (),
                    Some(crate::Interpolation::Flat) => {
                        self.decorate(id, Decoration::Flat, &[]);
                    }
                    Some(crate::Interpolation::Linear) => {
                        self.decorate(id, Decoration::NoPerspective, &[]);
                    }
                }

                match sampling {
                    // Center sampling is the default in SPIR-V.
                    None | Some(crate::Sampling::Center) => (),
                    Some(crate::Sampling::Centroid) => {
                        self.decorate(id, Decoration::Centroid, &[]);
                    }
                    Some(crate::Sampling::Sample) => {
                        self.require_any(
                            "per-sample interpolation",
                            &[spirv::Capability::SampleRateShading],
                        )?;
                        self.decorate(id, Decoration::Sample, &[]);
                    }
                }
            }
            crate::Binding::BuiltIn(built_in) => {
                use crate::BuiltIn as Bi;
                let built_in = match built_in {
                    Bi::Position => {
                        if class == spirv::StorageClass::Output {
                            BuiltIn::Position
                        } else {
                            BuiltIn::FragCoord
                        }
                    }
                    Bi::ViewIndex => {
                        self.require_any("`view_index` built-in", &[spirv::Capability::MultiView])?;
                        BuiltIn::ViewIndex
                    }
                    // vertex
                    Bi::BaseInstance => BuiltIn::BaseInstance,
                    Bi::BaseVertex => BuiltIn::BaseVertex,
                    Bi::ClipDistance => BuiltIn::ClipDistance,
                    Bi::CullDistance => BuiltIn::CullDistance,
                    Bi::InstanceIndex => BuiltIn::InstanceIndex,
                    Bi::PointSize => BuiltIn::PointSize,
                    Bi::VertexIndex => BuiltIn::VertexIndex,
                    // fragment
                    Bi::FragDepth => BuiltIn::FragDepth,
                    Bi::FrontFacing => BuiltIn::FrontFacing,
                    Bi::PrimitiveIndex => {
                        self.require_any(
                            "`primitive_index` built-in",
                            &[spirv::Capability::Geometry],
                        )?;
                        BuiltIn::PrimitiveId
                    }
                    Bi::SampleIndex => {
                        self.require_any(
                            "`sample_index` built-in",
                            &[spirv::Capability::SampleRateShading],
                        )?;

                        BuiltIn::SampleId
                    }
                    Bi::SampleMask => BuiltIn::SampleMask,
                    // compute
                    Bi::GlobalInvocationId => BuiltIn::GlobalInvocationId,
                    Bi::LocalInvocationId => BuiltIn::LocalInvocationId,
                    Bi::LocalInvocationIndex => BuiltIn::LocalInvocationIndex,
                    Bi::WorkGroupId => BuiltIn::WorkgroupId,
                    Bi::WorkGroupSize => BuiltIn::WorkgroupSize,
                    Bi::NumWorkGroups => BuiltIn::NumWorkgroups,
                };

                self.decorate(id, Decoration::BuiltIn, &[built_in as u32]);
            }
        }

        Ok(id)
    }

    fn write_global_variable(
        &mut self,
        ir_module: &crate::Module,
        global_variable: &crate::GlobalVariable,
    ) -> Result<(Instruction, Word), Error> {
        let id = self.id_gen.next();

        let class = map_storage_class(global_variable.class);
        //self.check(class.required_capabilities())?;

        let init_word = global_variable
            .init
            .map(|constant| self.constant_ids[constant.index()]);
        let pointer_type_id = self.get_pointer_id(&ir_module.types, global_variable.ty, class)?;
        let instruction = Instruction::variable(pointer_type_id, id, class, init_word);

        if self.flags.contains(WriterFlags::DEBUG) {
            if let Some(ref name) = global_variable.name {
                self.debugs.push(Instruction::name(id, name));
            }
        }

        use spirv::Decoration;

        let storage_access = match global_variable.class {
            crate::StorageClass::Storage { access } => Some(access),
            _ => match ir_module.types[global_variable.ty].inner {
                crate::TypeInner::Image {
                    class: crate::ImageClass::Storage { access, .. },
                    ..
                } => Some(access),
                _ => None,
            },
        };
        if let Some(storage_access) = storage_access {
            if !storage_access.contains(crate::StorageAccess::LOAD) {
                self.decorate(id, Decoration::NonReadable, &[]);
            }
            if !storage_access.contains(crate::StorageAccess::STORE) {
                self.decorate(id, Decoration::NonWritable, &[]);
            }
        }

        if let Some(ref res_binding) = global_variable.binding {
            self.decorate(id, Decoration::DescriptorSet, &[res_binding.group]);
            self.decorate(id, Decoration::Binding, &[res_binding.binding]);
        }

        // TODO Initializer is optional and not (yet) included in the IR
        Ok((instruction, id))
    }

    fn get_function_type(&mut self, lookup_function_type: LookupFunctionType) -> Word {
        match self
            .lookup_function_type
            .entry(lookup_function_type.clone())
        {
            Entry::Occupied(e) => *e.get(),
            _ => {
                let id = self.id_gen.next();
                let instruction = Instruction::type_function(
                    id,
                    lookup_function_type.return_type_id,
                    &lookup_function_type.parameter_type_ids,
                );
                instruction.to_words(&mut self.logical_layout.declarations);
                self.lookup_function_type.insert(lookup_function_type, id);
                id
            }
        }
    }

    fn write_physical_layout(&mut self) {
        self.physical_layout.bound = self.id_gen.0 + 1;
    }

    fn write_logical_layout(
        &mut self,
        ir_module: &crate::Module,
        mod_info: &ModuleInfo,
        ep_index: Option<usize>,
    ) -> Result<(), Error> {
        fn has_view_index_check(
            ir_module: &crate::Module,
            binding: Option<&crate::Binding>,
            ty: Handle<crate::Type>,
        ) -> bool {
            match ir_module.types[ty].inner {
                crate::TypeInner::Struct { ref members, .. } => members.iter().any(|member| {
                    has_view_index_check(ir_module, member.binding.as_ref(), member.ty)
                }),
                _ => binding == Some(&crate::Binding::BuiltIn(crate::BuiltIn::ViewIndex)),
            }
        }

        let has_storage_buffers =
            ir_module
                .global_variables
                .iter()
                .any(|(_, var)| match var.class {
                    crate::StorageClass::Storage { .. } => true,
                    _ => false,
                });
        let has_view_index = ir_module
            .entry_points
            .iter()
            .flat_map(|entry| entry.function.arguments.iter())
            .any(|arg| has_view_index_check(ir_module, arg.binding.as_ref(), arg.ty));

        if self.physical_layout.version < 0x10300 && has_storage_buffers {
            // enable the storage buffer class on < SPV-1.3
            Instruction::extension("SPV_KHR_storage_buffer_storage_class")
                .to_words(&mut self.logical_layout.extensions);
        }
        if has_view_index {
            Instruction::extension("SPV_KHR_multiview")
                .to_words(&mut self.logical_layout.extensions)
        }
        Instruction::type_void(self.void_type).to_words(&mut self.logical_layout.declarations);
        Instruction::ext_inst_import(self.gl450_ext_inst_id, "GLSL.std.450")
            .to_words(&mut self.logical_layout.ext_inst_imports);

        if self.flags.contains(WriterFlags::DEBUG) {
            self.debugs
                .push(Instruction::source(spirv::SourceLanguage::GLSL, 450));
        }

        self.constant_ids.resize(ir_module.constants.len(), 0);
        // first, output all the scalar constants
        for (handle, constant) in ir_module.constants.iter() {
            match constant.inner {
                crate::ConstantInner::Composite { .. } => continue,
                crate::ConstantInner::Scalar { width, ref value } => {
                    self.constant_ids[handle.index()] = match constant.name {
                        Some(ref name) => {
                            let id = self.id_gen.next();
                            self.write_constant_scalar(id, value, width, Some(name));
                            id
                        }
                        None => self.get_constant_scalar(*value, width),
                    };
                }
            }
        }

        // then all types, some of them may rely on constants and struct type set
        for (handle, _) in ir_module.types.iter() {
            self.write_type_declaration_arena(&ir_module.types, handle)?;
        }

        // the all the composite constants, they rely on types
        for (handle, constant) in ir_module.constants.iter() {
            match constant.inner {
                crate::ConstantInner::Scalar { .. } => continue,
                crate::ConstantInner::Composite { ty, ref components } => {
                    let id = self.id_gen.next();
                    self.constant_ids[handle.index()] = id;
                    if self.flags.contains(WriterFlags::DEBUG) {
                        if let Some(ref name) = constant.name {
                            self.debugs.push(Instruction::name(id, name));
                        }
                    }
                    self.write_constant_composite(id, ty, components)?;
                }
            }
        }
        debug_assert_eq!(self.constant_ids.iter().position(|&id| id == 0), None);

        // now write all globals
        for (handle, var) in ir_module.global_variables.iter() {
            // If a single entry point was specified, only write `OpVariable` instructions
            // for the globals it actually uses. Emit dummies for the others,
            // to preserve the indices in `global_variables`.
            let gvar = match ep_index {
                Some(index) if mod_info.get_entry_point(index)[handle].is_empty() => {
                    GlobalVariable::dummy()
                }
                _ => {
                    let (instruction, id) = self.write_global_variable(ir_module, var)?;
                    instruction.to_words(&mut self.logical_layout.declarations);
                    GlobalVariable::new(id)
                }
            };
            self.global_variables.push(gvar);
        }

        // all functions
        for (handle, ir_function) in ir_module.functions.iter() {
            let info = &mod_info[handle];
            if let Some(index) = ep_index {
                let ep_info = mod_info.get_entry_point(index);
                // If this function uses globals that we omitted from the SPIR-V
                // because the entry point and its callees didn't use them,
                // then we must skip it.
                if !ep_info.dominates_global_use(info) {
                    log::info!("Skip function {:?}", ir_function.name);
                    continue;
                }
            }
            let id = self.write_function(ir_function, info, ir_module, None)?;
            self.lookup_function.insert(handle, id);
        }

        // and entry points
        for (index, ir_ep) in ir_module.entry_points.iter().enumerate() {
            if ep_index.is_some() && ep_index != Some(index) {
                continue;
            }
            let info = mod_info.get_entry_point(index);
            let ep_instruction = self.write_entry_point(ir_ep, info, ir_module)?;
            ep_instruction.to_words(&mut self.logical_layout.entry_points);
        }

        for capability in self.capabilities_used.iter() {
            Instruction::capability(*capability).to_words(&mut self.logical_layout.capabilities);
        }
        if ir_module.entry_points.is_empty() {
            // SPIR-V doesn't like modules without entry points
            Instruction::capability(spirv::Capability::Linkage)
                .to_words(&mut self.logical_layout.capabilities);
        }

        let addressing_model = spirv::AddressingModel::Logical;
        let memory_model = spirv::MemoryModel::GLSL450;
        //self.check(addressing_model.required_capabilities())?;
        //self.check(memory_model.required_capabilities())?;

        Instruction::memory_model(addressing_model, memory_model)
            .to_words(&mut self.logical_layout.memory_model);

        if self.flags.contains(WriterFlags::DEBUG) {
            for debug in self.debugs.iter() {
                debug.to_words(&mut self.logical_layout.debugs);
            }
        }

        for annotation in self.annotations.iter() {
            annotation.to_words(&mut self.logical_layout.annotations);
        }

        Ok(())
    }

    pub fn write(
        &mut self,
        ir_module: &crate::Module,
        info: &ModuleInfo,
        pipeline_options: Option<&PipelineOptions>,
        words: &mut Vec<Word>,
    ) -> Result<(), Error> {
        self.reset();

        // Try to find the entry point and corresponding index
        let ep_index = match pipeline_options {
            Some(po) => {
                let index = ir_module
                    .entry_points
                    .iter()
                    .position(|ep| po.shader_stage == ep.stage && po.entry_point == ep.name)
                    .ok_or(Error::EntryPointNotFound)?;
                Some(index)
            }
            None => None,
        };

        self.write_logical_layout(ir_module, info, ep_index)?;
        self.write_physical_layout();

        self.physical_layout.in_words(words);
        self.logical_layout.in_words(words);
        Ok(())
    }

    /// Return the set of capabilities the last module written used.
    pub fn get_capabilities_used(&self) -> &crate::FastHashSet<spirv::Capability> {
        &self.capabilities_used
    }
}

#[test]
fn test_write_physical_layout() {
    let mut writer = Writer::new(&Options::default()).unwrap();
    assert_eq!(writer.physical_layout.bound, 0);
    writer.write_physical_layout();
    assert_eq!(writer.physical_layout.bound, 3);
}
