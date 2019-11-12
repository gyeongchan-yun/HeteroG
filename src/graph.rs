use oh_my_rust::*;
use protobuf::Message;
use crate::proto::{graph::GraphDef, node_def::NodeDef, attr_value::AttrValue, types::DataType};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::convert::TryInto;
use crate::strategy::Strategy;

pub struct Graph<NEX: Default, TEX: Default> {
    pub nodes: Vec<Node<NEX, TEX>>, // This vector is partial ordered: inputs are guarenteed to appear ealier than descendents
    pub name_dict: std::collections::BTreeMap<String, usize>
}

impl<NEX: Default, TEX: Default> Graph<NEX, TEX> {
    pub fn new(nodes: &[NodeDef]) -> Box<Self> {
        task!("building graph of {} nodes...", nodes.len());

        let mut g = Box::new(Graph { nodes: Vec::with_capacity(nodes.len()), name_dict: BTreeMap::new() });

        // no always optimal, but good enough since the input is actually mostly ordered
        let mut queue: std::collections::VecDeque::<_> = nodes.iter().collect();
        'outer: while let Some(node_def) = queue.pop_front() {
            for input in node_def.input.iter() {
                let input = if input.starts_with('^') {
                    &input[1..]
                } else {
                    parse_input(input).0
                };
                if !g.name_dict.contains_key(input) {
                    debug!("pushing back {}", node_def.name);
                    queue.push_back(node_def);
                    continue 'outer;
                }
            }

            let node = Node::new(&g, node_def.clone());
            g.name_dict.insert(node.raw_node.name.clone(), g.nodes.len());
            g.nodes.push(node);
        }

        g
    }

    /// setup the replicas and links. Note that auxiliary nodes are already there by strategies.
    pub fn compile(&mut self, target: &mut Target) {
        task!("compiling graph of {} nodes...", self.nodes.len());
        for node in self.nodes.iter_mut() {
            node.compile(target)
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Copy)]
pub enum FormKind { Full, Part }

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct Form {
    pub kind: FormKind,
    pub devices: Vec<usize> // The Vec must be sorted and not empty, but may contains repeated elements (put multiple replicas on the same device)
}

impl Form {
    pub fn is_full(&self) -> bool {
        self.kind == FormKind::Full
    }

    pub fn is_part(&self) -> bool {
        self.kind == FormKind::Part
    }

    pub fn ndev(&self) -> usize {
        self.devices.len()
    }

    // TODO: use to_string() and parse()?
    pub fn code(&self) -> String {
        let mut x = String::from(if self.is_full() {"full"} else {"part"});
        for d in self.devices.iter() {
            x += "_";
            x += &d.to_string();
        }
        x
    }

    pub fn from_code(code: &str) -> Self {
        let segs: Vec<_> = code.split('_').collect();
        let kind = match segs[0] {
            "full" => FormKind::Full,
            "part" => FormKind::Part,
            _ => unreachable!()
        };
        Self { kind, devices: segs[1..].iter().map(|x| x.parse().unwrap()).collect() }
    }

    pub fn valid(&self) -> bool {
        !self.devices.is_empty()
    }
}

pub struct Node<NEX: Default, TEX: Default> {
    pub graph: *const Graph<NEX, TEX>,
    pub raw_node: NodeDef,
    pub controls: Vec<usize>, // TODO: more consideration for control dependencies that added aux nodes
    pub inputs: Vec<(usize, usize, FormKind)>, // nodeid, index, formkind (defaults to full)
    pub outputs: Vec<Tensor<NEX, TEX>>,
    pub form: Form, // the form of the node, which is also a tensor form for all its outputs

    pub extra: NEX
}

impl<NEX: Default, TEX: Default> Node<NEX, TEX> {
    pub fn new(graph: &Graph<NEX, TEX>, raw_node: NodeDef) -> Self {
        let mut inputs = vec![];
        let mut controls = vec![];

        for input in raw_node.input.iter() {
            if input.starts_with('^') {
                controls.push(graph.name_dict[&input[1..]])
            } else {
                let (name, index) = parse_input(input);
                let id = graph.name_dict[name];
                inputs.push((id, index, FormKind::Full))
            }
        }

        Self {
            graph, raw_node, controls, inputs, outputs: vec![],
            form: Form { kind: FormKind::Full, devices: vec![] },
            extra: Default::default()
        }
    }

    #[allow(clippy::mut_from_ref)]
    pub fn graph<'a>(&self) -> &'a mut Graph<NEX, TEX> {
        unsafe { &mut *(self.graph as *mut Graph<NEX, TEX>) }
    }

    #[allow(clippy::mut_from_ref, clippy::cast_ref_to_mut)]
    pub fn get_output(&self, index: usize) -> &mut Tensor<NEX, TEX> {
        let mutable = unsafe { &mut *(self as *const Node<NEX, TEX> as *mut Node<NEX, TEX>) };

        while mutable.outputs.len() <= index {
            mutable.outputs.push(Tensor::new(mutable, mutable.outputs.len()))
        }

        &mut mutable.outputs[index]
    }

    pub fn replicated(&self) -> Option<bool> {
        match self.form.ndev() {
            0 => None,
            1 => Some(false),
            _ => Some(true)
        }
    }

    /// add an edited node into the target. Requires all inputs to be compiled first
    fn compile(&mut self, target: &mut Target) {
        debug!("compile: {} {:?} {:?}", self.raw_node.name, self.form, self.inputs);

        for (replica_index, device_id) in self.form.devices.iter().enumerate() {
            // 1. setup basic node info
            let mut node = self.raw_node.clone();
            node.name = self.replica(replica_index);
            node.device = target.devices[*device_id].clone();
            set_origin(&mut node, &self.raw_node.name);
            set_form(&mut node, &self.form.code());

            // 2. link inputs and set size
            node.input = self.inputs.iter().copied().enumerate().map(|(i, (node_id, index, kind))| {
                let input_tensor = &mut self.graph().nodes[node_id].get_output(index);
                set_input_size(&mut node, i, match self.form.kind {
                    FormKind::Full => input_tensor.get_size(),
                    FormKind::Part => input_tensor.get_size() / self.form.ndev() as u64,
                });
                let input_names = input_tensor.as_form(&Form { kind, devices: self.form.devices.clone() }, target);
                input_names[replica_index].clone()
            }).collect();

            // 3. add control dependencies
            for node_id in self.controls.iter() {
                let dep_node = &self.graph().nodes[*node_id];
                for i in 0..dep_node.form.ndev() {
                    node.input.push(dep_node.replica(i))
                }
            }

            target.pb.node.push(node)
        }
    }

    fn replica(&self, index: usize) -> String { // TODO: should this method exist?
        format!("{}/replica_{}", self.raw_node.name, index)
    }

    /**************************************
    * following are graph editing methods *
    **************************************/

    pub fn make_node(&self, op: String) -> NodeDef {
        let mut node = NodeDef::new();
        node.op = op;
        node.name = self.raw_node.name.clone();
        set_belong_to(&mut node, &self.raw_node.name);
        node
    }

    pub fn put_on_devices(&mut self, devices: &[usize]) {
        assert!(self.replicated().is_none(), "already set replicas!");
        self.form.devices.extend_from_slice(devices);
    }
}

pub struct Tensor<NEX: Default, TEX: Default> {
    pub node: *const Node<NEX, TEX>,
    pub index: usize,
    pub forms: BTreeMap<Form, Box<[String]>>,

    pub extra: TEX,
}

impl<NEX: Default, TEX: Default> Tensor<NEX, TEX> {
    pub fn new(node: &Node<NEX, TEX>, index: usize) -> Self {
        Tensor { node, index, forms: BTreeMap::new(), extra: TEX::default() }
    }

    pub fn original_name(&self) -> String {
        if self.index == 0 {
            self.node().raw_node.name.clone()
        } else {
            format!("{}:{}", self.node().raw_node.name, self.index)
        }
    }

    pub fn node<'a>(&self) -> &'a Node<NEX, TEX> {
        unsafe { &*self.node }
    }

    pub fn get_shape(&self) -> Vec<usize> {
        // sucks: the output shape of BroadcastGradientArgs is always unknown even if inputs are fixed
        // and ops like `Sum` (requires the dimension to sum along with) and `Fill` operates differently with different inputs
        self.node().raw_node.attr["_output_shapes"].get_list().shape[self.index].dim.iter().map(|x| x.size.try_into().ok()).collect::<Option<_>>().unwrap_or_else(Vec::new)
    }

    pub fn get_size(&self) -> u64 {
        #[allow(clippy::unnecessary_fold)]
        (self.get_shape().iter().fold(1, |x, y| x * y) * 4).try_into().unwrap()
    }

    // get the names as the specified form
    pub fn as_form(&mut self, form: &Form, target: &mut Target) -> &[String] {
        if !self.forms.contains_key(form) {
            let names = if form == &self.node().form {
                (0..form.ndev()).map(|i| format!("{}:{}", self.node().replica(i), self.index)).collect()
            } else {
                let node_kind = self.node().form.kind;
                match (form.kind, node_kind) {
                    (FormKind::Full, FormKind::Full) => self.replicate_broadcast(&self.node().form, form, target),
                    (FormKind::Part, FormKind::Full) => self.replicate_split(&self.node().form, form, target),
                    (FormKind::Full, FormKind::Part) => self.aggregate_cat(&self.node().form, form, target),
                    (FormKind::Part, FormKind::Part) => self.resplit(&self.node().form, form, target),
                }
            };

            self.forms.insert(form.clone(), names);
        }

        &self.forms[form]
    }

    /**************************************
    * following are graph editing methods *
    **************************************/

    pub fn aggregate_sum(&mut self, from: &Form, to: &Form, target: &mut Target) -> Box<[String]> {
        assert!(from.valid() && to.valid() && from.is_part() && to.is_full());

        let mut addn = self.node().make_node("AddN".to_string());
        addn.name += &format!("/{}_{}/aux_sum", self.index, to.code());
        addn.device = target.devices[to.devices[0]].clone();
        addn.attr.insert("N".into(), AttrValue::new().apply(|x| x.set_i(from.ndev().try_into().unwrap())));
        addn.attr.insert("T".into(), get_dtype(&self.node().raw_node));
        addn.input = self.as_form(from, target).iter().cloned().collect();
        for i in 0..from.ndev() {
            set_input_size(&mut addn, i, self.get_size() / from.ndev() as u64)
        }

        let result = vec![addn.name.clone(); to.ndev()].into_boxed_slice();
        target.pb.node.push(addn);
        result
    }

    // TODO: share the same axis nodes for all concating (and do the same thing for dim nodes in splitting)
    pub fn aggregate_cat(&mut self, from: &Form, to: &Form, target: &mut Target) -> Box<[String]> {
        assert!(from.valid() && to.valid() && from.is_part() && to.is_full());

        let mut axis = self.node().make_node("Const".to_string());
        axis.name += &format!("/{}_{}/aux_concat/axis", self.index, to.code());
        axis.device = target.devices[to.devices[0]].clone();
        axis.attr.insert("dtype".into(), AttrValue::new().apply(|x| x.set_field_type(DataType::DT_INT32)));
        let value = crate::proto::tensor::TensorProto::new().apply(|x| {
            x.set_dtype(DataType::DT_INT32);
            x.set_tensor_shape(crate::proto::tensor_shape::TensorShapeProto::new());
            x.int_val.push(0);
        });
        axis.attr.insert("value".into(), AttrValue::new().apply(|x| x.set_tensor(value)));

        let mut concat = self.node().make_node("ConcatV2".to_string());
        concat.name += &format!("/{}_{}/aux_concat/concat", self.index, to.code());
        concat.device = target.devices[to.devices[0]].clone();
        concat.input = self.as_form(from, target).iter().cloned().collect();
        concat.input.push(axis.name.clone());
        concat.attr.insert("N".into(), AttrValue::new().apply(|x| x.set_i(from.ndev().try_into().unwrap())));
        concat.attr.insert("T".into(), get_dtype(&self.node().raw_node));
        concat.attr.insert("Tidx".into(), AttrValue::new().apply(|x| x.set_field_type(DataType::DT_INT32)));
        for i in 0..from.ndev() {
            set_input_size(&mut concat, i, self.get_size() / from.ndev() as u64)
        }

        let result = vec![concat.name.clone(); to.ndev()].into_boxed_slice();
        target.pb.node.push(axis);
        target.pb.node.push(concat);
        result
    }

    pub fn replicate_broadcast(&mut self, from: &Form, to: &Form, target: &mut Target) -> Box<[String]> {
        assert!(from.valid() && to.valid() && from.is_full() && to.is_full());

        let raw = self.as_form(&self.node().form, target).to_vec(); // TODO: no clone?
        to.devices.iter().map(|device_id| {
            from.devices.iter().position(|x| *x == *device_id).map(|ind| raw[ind].clone()).unwrap_or_else(|| raw[0].clone())
        }).collect()
    }

    // currenly we only split from the first replica. Future we can split on every device and use the local copy to reduce transfering
    pub fn replicate_split(&mut self, from: &Form, to: &Form, target: &mut Target) -> Box<[String]> {
        assert!(from.valid() && to.valid() && from.is_full() && to.is_part());

        let mut dim = self.node().make_node("Const".to_string());
        dim.name += &format!("/{}_{}/aux_split/dim", self.index, to.code());
        dim.device = target.devices[from.devices[0]].clone();
        dim.attr.insert("dtype".into(), AttrValue::new().apply(|x| x.set_field_type(DataType::DT_INT32)));
        let value = crate::proto::tensor::TensorProto::new().apply(|x| {
            x.set_dtype(DataType::DT_INT32);
            x.set_tensor_shape(crate::proto::tensor_shape::TensorShapeProto::new());
            x.int_val.push(0);
        });
        dim.attr.insert("value".into(), AttrValue::new().apply(|x| x.set_tensor(value)));

        let mut split = self.node().make_node("Split".to_string());
        split.name += &format!("/{}_{}/aux_split/split", self.index, to.code());
        split.device = target.devices[from.devices[0]].clone();
        split.input.push(dim.name.clone());
        split.input.push(self.as_form(from, target)[0].clone());
        split.attr.insert("T".into(), get_dtype(&self.node().raw_node));
        split.attr.insert("num_split".into(), AttrValue::new().apply(|x| x.set_i(to.ndev().try_into().unwrap())));
        set_input_size(&mut split, 1, self.get_size());

        let result = (0..to.ndev()).map(|i| format!("{}:{}", split.name, i)).collect();
        target.pb.node.push(dim);
        target.pb.node.push(split);
        result
    }

    pub fn resplit(&mut self, from: &Form, to: &Form, target: &mut Target) -> Box<[String]> {
        assert!(from.valid() && to.valid() && from.is_part() && to.is_part());

        let gcd = { // the number of intermediat concated nodes
            let mut a = from.ndev();
            let mut b = to.ndev();
            while a != b {
                if a > b {
                    a -= b;
                } else {
                    b -= a;
                }
            }
            a
        };

        self.as_form(from, target).to_vec().chunks(from.ndev() / gcd).enumerate().map(|(i, chunk)| {
            let dest = from.devices[i * chunk.len()];

            let mut axis = self.node().make_node("Const".to_string());
            axis.name += &format!("/{}_{}/aux_resplit_{}/concat_axis", self.index, to.code(), i);
            axis.device = target.devices[dest].clone();
            axis.attr.insert("dtype".into(), AttrValue::new().apply(|x| x.set_field_type(DataType::DT_INT32)));
            let value = crate::proto::tensor::TensorProto::new().apply(|x| {
                x.set_dtype(DataType::DT_INT32);
                x.set_tensor_shape(crate::proto::tensor_shape::TensorShapeProto::new());
                x.int_val.push(0);
            });
            axis.attr.insert("value".into(), AttrValue::new().apply(|x| x.set_tensor(value)));

            let mut concat = self.node().make_node("ConcatV2".to_string());
            concat.name += &format!("/{}_{}/aux_resplit_{}/concat", self.index, to.code(), i);
            concat.device = target.devices[dest].clone();
            concat.input = chunk.iter().cloned().collect();
            concat.input.push(axis.name.clone());
            concat.attr.insert("N".into(), AttrValue::new().apply(|x| x.set_i(chunk.len().try_into().unwrap())));
            concat.attr.insert("T".into(), get_dtype(&self.node().raw_node));
            concat.attr.insert("Tidx".into(), AttrValue::new().apply(|x| x.set_field_type(DataType::DT_INT32)));
            for j in 0..chunk.len() {
                set_input_size(&mut concat, j, self.get_size() / from.ndev() as u64)
            }

            let result = concat.name.clone();
            target.pb.node.push(axis);
            target.pb.node.push(concat);
            (dest, result)
        }).collect::<Vec<_>>().iter().zip(to.devices.chunks(to.ndev() / gcd)).enumerate().flat_map(|(i, ((concat_place, concated), devices))| {
            let mut dim = self.node().make_node("Const".to_string());
            dim.name += &format!("/{}_{}/aux_resplit_{}/split_dim", self.index, to.code(), i);
            dim.device = target.devices[*concat_place].clone();
            dim.attr.insert("dtype".into(), AttrValue::new().apply(|x| x.set_field_type(DataType::DT_INT32)));
            let value = crate::proto::tensor::TensorProto::new().apply(|x| {
                x.set_dtype(DataType::DT_INT32);
                x.set_tensor_shape(crate::proto::tensor_shape::TensorShapeProto::new());
                x.int_val.push(0);
            });
            dim.attr.insert("value".into(), AttrValue::new().apply(|x| x.set_tensor(value)));

            let mut split = self.node().make_node("Split".to_string());
            split.name += &format!("/{}_{}/aux_resplit_{}/split", self.index, to.code(), i);
            split.device = target.devices[*concat_place].clone();
            split.input.push(dim.name.clone());
            split.input.push(concated.clone());
            split.attr.insert("T".into(), get_dtype(&self.node().raw_node));
            split.attr.insert("num_split".into(), AttrValue::new().apply(|x| x.set_i(devices.len().try_into().unwrap())));
            set_input_size(&mut split, 1, self.get_size() / gcd as u64);

            let result = (0..to.ndev() / gcd).map({
                let name = split.name.clone();
                move |i| format!("{}:{}", name, i)
            });
            target.pb.node.push(dim);
            target.pb.node.push(split);
            result
        }).collect()
    }

    pub fn all_reduce_nccl(&mut self, from: &Form, to: &Form, target: &mut Target) -> Box<[String]> {
        // to all_sum n tensors (can be on the same devie), one should have n NcclAllReduce nodes with the same shared_name attr
        // each node have only *one* input, and should be on the same device of the input. The output of these nodes will be the same

        assert!(from.valid() && to.valid() && from.is_part() && to.is_full() && from.devices == to.devices);

        let index = self.index;

        for (i, device_id) in from.devices.iter().copied().enumerate() {
            let mut nccl = self.node().make_node("NcclAllReduce".to_string());
            nccl.name += &format!("/{}_{}/aux_nccl_{}", index, to.code(), i);
            nccl.device = target.devices[device_id].clone();
            nccl.attr.insert("reduction".into(), AttrValue::new().apply(|x| x.set_s(b"sum".to_vec())));
            nccl.attr.insert("T".into(), get_dtype(&self.node().raw_node));
            nccl.attr.insert("num_devices".into(), AttrValue::new().apply(|x| x.set_i(from.ndev().try_into().unwrap())));
            nccl.attr.insert("shared_name".into(), AttrValue::new().apply(|x| x.set_s(self.original_name().into_bytes())));
            nccl.input.push(format!("{}:{}", self.as_form(from, target)[i], index));

            target.pb.node.push(nccl)
        }

        (0..from.ndev()).map(|i| format!("{}/{}_{}/aux_nccl_{}", self.node().raw_node.name, self.index, to.code(), i)).collect()
    }

    pub fn all_reduce_ring(&mut self, from: &Form, to: &Form, target: &mut Target) -> Box<[String]> {
        assert!(from.valid() && to.valid() && from.is_part() && to.is_full() && from.devices == to.devices);

        let devices: Vec<_> = from.devices.iter().map(|id| target.devices[*id].clone()).collect();
        let n = devices.len();
        let dtype = get_dtype(&self.node().raw_node);
        let psize = self.get_size() / from.ndev() as u64;
        let list = self.as_form(from, target).to_vec();

        // 1. recording the shape
        let shapes: Vec<_> = (0..n).map(|i| {
            let mut shape = self.node().make_node("Shape".to_string());
            shape.name += &format!("/{}_{}/aux_ring/shape_{}", to.code(), self.index, i);
            shape.device = devices[i].clone();
            shape.attr.insert("T".into(), dtype.clone());
            shape.input.push(list[i].clone());
            set_input_size(&mut shape, 0, psize);
            let ret = shape.name.clone();
            target.pb.node.push(shape);
            ret
        }).collect();

        // 2. flattening
        let flats: Vec<_> = (0..n).map(|i| {
            let mut shape = self.node().make_node("Const".to_string());
            shape.name += &format!("/{}_{}/aux_ring/flat_{}/shape", to.code(), self.index, i);
            shape.device = devices[i].clone();
            shape.attr.insert("dtype".into(), AttrValue::new().apply(|x| x.set_field_type(DataType::DT_INT32)));
            let mut value = crate::proto::tensor::TensorProto::new();
            let mut x = crate::proto::tensor_shape::TensorShapeProto::new();
            let mut dim = crate::proto::tensor_shape::TensorShapeProto_Dim::new();
            dim.size = 1;
            x.dim.push(dim);
            value.dtype = DataType::DT_INT32;
            value.tensor_shape = protobuf::SingularPtrField::some(x);
            value.int_val.push(-1);
            shape.attr.insert("value".into(), AttrValue::new().apply(|x| x.set_tensor(value)));

            let mut flat = self.node().make_node("Reshape".to_string());
            flat.name += &format!("/{}_{}/aux_ring/flat_{}/flat", to.code(), self.index, i);
            flat.device = devices[i].clone();
            flat.attr.insert("T".into(), dtype.clone());
            flat.input.push(list[i].clone());
            flat.input.push(shape.name.clone());
            set_input_size(&mut flat, 0, psize);

            let ret = flat.name.clone();
            target.pb.node.push(shape);
            target.pb.node.push(flat);
            ret
        }).collect();

        // 3. chunking
        let mut chunks: Vec<Vec<String>> = (0..n).map(|i| {
            let mut dim = self.node().make_node("Const".to_string());
            dim.name += &format!("/{}_{}/aux_ring/split_{}/dim", to.code(), self.index, i);
            dim.device = devices[i].clone();
            dim.attr.insert("dtype".into(), AttrValue::new().apply(|x| x.set_field_type(DataType::DT_INT32)));
            let mut value = crate::proto::tensor::TensorProto::new();
            let shape = crate::proto::tensor_shape::TensorShapeProto::new();
            value.dtype = DataType::DT_INT32;
            value.tensor_shape = protobuf::SingularPtrField::some(shape);
            value.int_val.push(0);
            dim.attr.insert("value".into(), AttrValue::new().apply(|x| x.set_tensor(value)));

            let mut split = self.node().make_node("Split".to_string());
            split.name += &format!("/{}_{}/aux_ring/split_{}/split", to.code(), self.index, i);
            split.device = devices[i].clone();
            split.input.push(dim.name.clone());
            split.input.push(flats[i].clone());
            split.attr.insert("T".into(), dtype.clone());
            split.attr.insert("num_split".into(), AttrValue::new().apply(|x| x.set_i(n.try_into().unwrap())));
            set_input_size(&mut split, 1, psize);

            let ret = split.name.clone();
            target.pb.node.push(dim);
            target.pb.node.push(split);

            (0..n).map(|x| format!("{}:{}", ret, x)).collect()
        }).collect();

        // 4. n-1 rounds of reducing. the last modified chunks (i+n-2) have the full content
        for round in 0..n-1 {
            // at the r round, the r+i chunk on i node is replaced by the sum of r+i and r+i+1
            for i in 0..n {
                let mut add = self.node().make_node("Add".to_string());
                add.name += &format!("/{}_{}/aux_ring/add_{}_{}", to.code(), self.index, i, round);
                add.device = devices[i].clone();
                add.input.push(chunks[i][(round+i) % n].clone());
                add.input.push(chunks[(i+1) % n][(round+i) % n].clone());
                add.attr.insert("T".into(), dtype.clone());
                set_input_size(&mut add, 0, psize);
                set_input_size(&mut add, 1, psize);
                chunks[i][(round+i) % n] = add.name.clone();
                target.pb.node.push(add);
            }
        }

        // 5. n-1 rounds of gathering
        for round in 0..n-1 {
            for i in 0..n {
                let mut identity = self.node().make_node("Identity".to_string());
                identity.name += &format!("/{}_{}/aux_ring/identity_{}_{}", to.code(), self.index, i, round);
                identity.device = devices[i].clone();
                identity.attr.insert("T".into(), dtype.clone());
                identity.input.push(chunks[(i+1) % n][(i+round+n-1) % n].clone());
                set_input_size(&mut identity, 0, psize);
                chunks[i][(i+round+n-1) % n] = identity.name.clone();
                target.pb.node.push(identity);
            }
        }

        // 6. concating
        let concated: Vec<_> = chunks.into_iter().enumerate().map(|(i, chunk)| {
            let mut axis = self.node().make_node("Const".to_string());
            axis.name += &format!("/{}_{}/aux_ring/concat_{}/axis", to.code(), self.index, i);
            axis.device = devices[i].clone();
            axis.attr.insert("dtype".into(), AttrValue::new().apply(|x| x.set_field_type(DataType::DT_INT32)));
            let mut value = crate::proto::tensor::TensorProto::new();
            let shape = crate::proto::tensor_shape::TensorShapeProto::new();
            value.dtype = DataType::DT_INT32;
            value.tensor_shape = protobuf::SingularPtrField::some(shape);
            value.int_val.push(0);
            axis.attr.insert("value".into(), AttrValue::new().apply(|x| x.set_tensor(value)));

            let len = chunk.len(); // save it here since we will destruct it later
            let mut concat = self.node().make_node("ConcatV2".to_string());
            concat.name += &format!("/{}_{}/aux_ring/concat_{}/concat", to.code(), self.index, i);
            concat.device = devices[i].clone();
            concat.input = chunk.into_iter().collect();
            concat.input.push(axis.name.clone());
            concat.attr.insert("N".into(), AttrValue::new().apply(|x| x.set_i(n.try_into().unwrap())));
            concat.attr.insert("T".into(), dtype.clone());
            concat.attr.insert("Tidx".into(), AttrValue::new().apply(|x| x.set_field_type(DataType::DT_INT32)));
            for j in 0..len {
                set_input_size(&mut concat, j, psize);
            }

            let ret = concat.name.clone();
            target.pb.node.push(axis);
            target.pb.node.push(concat);
            ret
        }).collect();

        // 7. restore shapes
        concated.into_iter().zip(shapes).enumerate().map(|(i, (concat, shape))| {
            let mut reshape = self.node().make_node("Reshape".to_string());
            reshape.name += &format!("/{}_{}/aux_ring/reshape_{}", to.code(), self.index, i);
            reshape.device = devices[i].clone();
            reshape.attr.insert("T".into(), dtype.clone());
            reshape.input.push(concat);
            reshape.input.push(shape);
            set_input_size(&mut reshape, 0, psize);

            let ret = reshape.name.clone();
            target.pb.node.push(reshape);
            ret
        }).collect()
    }
}

pub struct Target {
    pub pb: GraphDef,
    pub devices: Box<[String]>,
    pub links: Box<[u64]>, // the bandwidth of each link
    pub paths: Box<[Box<[usize]>]> // the i*n+j element is the links that i->j uses (currently only one path between each pair)
}

impl Target {
    pub fn new(pb: GraphDef, devices: Box<[String]>, links: Box<[u64]>, paths: Box<[Box<[usize]>]>) -> Self {
        Target { pb, devices, links, paths }
    }

    pub fn ndev(&self) -> usize {
        self.devices.len()
    }
}

fn set_origin(node: &mut NodeDef, origin: &str) {
    node.attr.insert("_tge_origin".to_string(), AttrValue::new().apply(|x| x.set_s(origin.as_bytes().to_vec())));
}

fn set_belong_to(node: &mut NodeDef, belong_to: &str) {
    node.attr.insert("_tge_belong_to".to_string(), AttrValue::new().apply(|x| x.set_s(belong_to.as_bytes().to_vec())));
}

fn set_input_size(node: &mut NodeDef, index: usize, size: u64) {
    let sizes = &mut node.attr.entry("_tge_input_sizes".to_string()).or_insert_with(AttrValue::new).mut_list().i;
    if sizes.len() <= index {
        sizes.resize(index+1, 0)
    }
    sizes[index] = size as _;
}

fn set_form(node: &mut NodeDef, form_code: &str) {
    node.attr.insert("_tge_form".to_string(), AttrValue::new().apply(|x| x.set_s(form_code.as_bytes().to_vec())));
}

// TODO: This function is not done. Need to parse ops.pbtxt and follow type or type_attr.
fn get_dtype(x: &NodeDef) -> AttrValue {
    match &x.op[..] {
        "Greater" | "GreaterEqual" => AttrValue::new().apply(|x| x.set_field_type(DataType::DT_BOOL)),
        "Shape" | "ShapeN" => x.attr.get("out_type").cloned().unwrap_or_else(|| AttrValue::new().apply(|x| x.set_field_type(DataType::DT_INT32))),
        "Cast" => x.attr.get("DstT").cloned().unwrap(),
        _ => x.attr.get("dtype").or_else(|| x.attr.get("T")).unwrap_or_else(|| panic!("cannot determine dtype for {}", x.op)).clone()
    }
}

fn parse_input(x: &str) -> (&str, usize) {
    match x.find(':') {
        Some(i) => (&x[..i], x[i+1..].parse().unwrap()),
        None => (x, 0)
    }
}
