use std::collections::HashMap;

use matrix::{self, ClMatrix, ClMatrixMode};

use super::operation::Operation;
use super::var_store::{VarIndex, VarStore};

#[derive(Clone)]
pub struct OutGrad {
    gradient: Option<VarIndex>, // The gradient or sum of gradients
    gradients: Vec<VarIndex>,
}

impl OutGrad {
    pub fn new() -> Self {
        OutGrad {
            gradient: None,
            gradients: vec![],
        }
    }

    pub fn gradient(&self) -> VarIndex {
        self.gradient.unwrap()
    }

    pub fn try_gradient(&self) -> Option<VarIndex> {
        self.gradient
    }

    fn maybe_sum(&self, ctx: &matrix::Context, var_store: &mut VarStore) {
        if self.gradients.len() > 0 {
            if let Some(sum) = self.gradient {
                var_store.get(self.gradients[0]).copy_to(ctx, &mut var_store.get_mut(sum));
                for grad in &self.gradients[1..] {
                    var_store.get(sum).add(ctx, &var_store.get(*grad), &mut var_store.get_mut(sum));
                }
            }
        }
    }

    fn fork(&mut self, ctx: &matrix::Context, var_store: &mut VarStore, v: VarIndex) {
        if self.gradients.len() > 0 {
            // There are multiple gradients already, just add the new one to the list
            self.gradients.push(v);
        } else if let Some(gradient) = self.gradient {
            // There is still only one gradient, switch it to a fork
            let (rows, columns) = {
                let grad = var_store.get(gradient);
                (grad.rows(), grad.columns())
            };
            // Create variable for gradient sum
            self.gradient = Some(var_store.add(ClMatrix::new(ctx, rows, columns, ClMatrixMode::Mut)));
            self.gradients.push(gradient);
            self.gradients.push(v);
        } else {
            // This is the only gradient so far, so we don't need to some anything
            self.gradient = Some(v);
        }
    }
}

pub struct Node {
    pub inputs: Vec<VarIndex>,
    pub outputs: Vec<VarIndex>,
    pub in_grad: Vec<VarIndex>, // gradients on inputs
    pub out_grad: Vec<OutGrad>, // gradients on outputs
}

pub struct Graph {
    nodes: Vec<Node>,
    node_ops: Vec<Box<Operation>>,
    pub var_store: VarStore,
    out_var_map: HashMap<VarIndex, (NodeIndex, usize)>, // Maps output variable to it's node and index within node
    // Gradients on variables that are inputs to the graph - they have no corresponding node
    in_var_grad: HashMap<VarIndex, OutGrad>,
}

impl Graph {
    pub fn new() -> Self {
        Graph {
            nodes: vec![],
            node_ops: vec![],
            var_store: VarStore::new(),
            out_var_map: HashMap::new(),
            in_var_grad: HashMap::new(),
        }
    }

    pub fn add_node(&mut self,
                    ctx: &matrix::Context,
                    op: Box<Operation>,
                    inputs: Vec<VarIndex>,
                    out_shapes: &[(u64, u64)])
                    -> NodeIndex {
        let node_index = NodeIndex(self.nodes.len());

        // Create output variables
        let mut outputs = vec![];
        for (i, &(rows, cols)) in out_shapes.iter().enumerate() {
            let var_index = self.var_store.add(ClMatrix::new(ctx, rows as usize, cols as usize, ClMatrixMode::Mut));
            outputs.push(var_index);
            self.out_var_map.insert(var_index, (node_index, i));
        }
        // Create input gradient variables and set up gradient back flow
        let mut in_grad = vec![];
        for input in &inputs {
            // Create input gradient variables
            let (rows, cols) = (input.get(self).rows(), input.get(self).columns());
            let var_index = self.var_store.add(ClMatrix::new(ctx, rows as usize, cols as usize, ClMatrixMode::Mut));
            in_grad.push(var_index);

            // Set up gradient back flow
            match self.out_var_map.get(input).map(|x| *x) {
                Some((in_node, out_index)) => {
                    self.nodes[in_node.0].out_grad[out_index].fork(ctx, &mut self.var_store, var_index);
                },
                None => {
                    // This input doesn't come from a node's output. It is an input to the graph.
                    self.in_var_grad.get_mut(input).unwrap()
                        .fork(ctx, &mut self.var_store, var_index);
                },
            }
        }
        // Create the node
        self.nodes.push(Node { inputs: inputs,
                               outputs: outputs,
                               in_grad: in_grad,
                               out_grad: vec![OutGrad::new(); out_shapes.len()] });
        // Add the corresponding node op
        self.node_ops.push(op);
        node_index
    }

    pub fn add_variable(&mut self, ctx: &matrix::Context, shape: (u64, u64)) -> VarIndex {
        let v = self.var_store.add(ClMatrix::new(ctx, shape.0 as usize, shape.1 as usize, ClMatrixMode::Mut));
        self.in_var_grad.insert(v, OutGrad::new());
        v
    }

    pub fn add_gradient(&mut self, ctx: &matrix::Context, n: NodeIndex, out_index: usize) -> VarIndex {
        let (rows, cols) = {
            let grad = n.get(self).outputs[out_index];
            let grad = grad.get(self);
            (grad.rows(), grad.columns())
        };
        let v = self.var_store.add(ClMatrix::new(ctx, rows as usize, cols as usize, ClMatrixMode::Mut));
        self.nodes[n.0].out_grad[out_index].fork(ctx, &mut self.var_store, v);
        v
    }

    pub fn run(&mut self, ctx: &matrix::Context) {
        // Forward pass
        //
        // NOTE: We just execute the nodes in order. We can do this because of the way the graph is
        // built. When a user wants to add a node, he/she must also supply the inputs. This means
        // any dependencies must already be added before the node can be added. Therefore, we can
        // assert that all dependents come after their dependencies in the `self.nodes` array.
        for (node, op) in self.nodes.iter_mut().zip(&mut self.node_ops) {
            op.forward(ctx, &mut self.var_store, node);
        }

        // Backward pass
        for (node, op) in self.nodes.iter_mut().rev().zip(self.node_ops.iter_mut().rev()) {
            // Sum the gradients on each output if there are multiple gradients
            for out_grad in &node.out_grad {
                out_grad.maybe_sum(ctx, &mut self.var_store);
            }
            op.backward(ctx, &mut self.var_store, node);
        }
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////

#[derive(Copy, Clone)]
pub struct NodeIndex(usize);

impl NodeIndex {
    pub fn get<'a>(&self, g: &'a Graph) -> &'a Node {
        &g.nodes[self.0]
    }

    pub fn get_mut<'a>(&self, g: &'a mut Graph) -> &'a mut Node {
        &mut g.nodes[self.0]
    }
}

#[test]
fn it_works() {
    use super::operation::MatMul;
    
    let ctx = matrix::Context::new();

    // Setup the graph
    let mut graph = Graph::new();
    let a = graph.add_variable(&ctx, (1, 2));
    let wa = graph.add_variable(&ctx, (2, 3));
    let node = graph.add_node(&ctx,
                              Box::new(MatMul::new(&ctx, (1, 2), (2, 3))),
                              vec![a, wa],
                              &[(1, 3)]);
    let node_g = graph.add_gradient(&ctx, node, 0);

    // Send some input data
    let a_cpu = matrix::Matrix::from_vec(1, 2, vec![1.0, 1.0]);
    let wa_cpu = matrix::Matrix::from_vec(2, 3, vec![0.5, 0.3, 0.2,
                                                     0.6, 0.7, 0.7]);
    let node_g_cpu = matrix::Matrix::from_vec(1, 3, vec![1.0, -1.0, 0.5]);
    a.get(&graph).set(&ctx, &a_cpu);
    wa.get(&graph).set(&ctx, &wa_cpu);
    node_g.get(&graph).set(&ctx, &node_g_cpu);

    // Run the network
    graph.run(&ctx);
    let out = node.get(&graph).outputs[0].get(&graph).get(&ctx);
    println!("{:?}", out);
    assert!(false);
}