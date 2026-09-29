use serde::{Deserialize, Serialize};

macro_rules! define_id {
    ($($name:ident),+ $(,)?) => {
        $(
            #[derive(
                Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd,
                Serialize, Deserialize,
            )]
            #[serde(transparent)]
            pub struct $name(pub u32);
        )+
    };
}

define_id!(
    FileId,
    SymbolId,
    FunctionId,
    BlockId,
    ValueId,
    SlotId,
    AllocationId,
    ContextId,
    ClosureId,
    ChoiceId,
    ModelId,
    EvidenceId,
);
