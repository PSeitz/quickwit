The Indexer serializes and packages splits inline before sending them to the Uploader.
The MergeExecutor packages merged splits inline, including compaction and delete-task
outputs. These stages share their owning actor's lifecycle, health checks, and failures;
there are no separate serialization or packaging actors or mailboxes. Serialization I/O
metrics now use the `indexer` component, and uploader backpressure applies directly to
the Indexer or MergeExecutor. Delete-task pipeline observations report packaging failures under `delete_task_executor` and no
longer include a `packager` entry.

```mermaid
flowchart LR
    subgraph Indexing pipeline
        direction LR
        publisher --inf--> source
        source[Source] --10--> doc_processor
        doc_processor[DocProcessor] --10--> indexer
        indexer[Indexer] --0--> uploader
        uploader[Uploader] --upload concurrency--> sequencer
        sequencer[Sequencer] --1--> publisher
    end
    subgraph Merge pipeline
        direction LR
        merge_downloader[MergeDownloader] --1--> merge_executor
        merge_executor[MergeExecutor] --0--> merge_uploader
        merge_uploader[MergeUploader] --inf--> merge_publisher
    end
    merge_planner[MergePlanner] --1--> merge_downloader
    merge_publisher[MergePublisher] --1--> merge_planner
    publisher[Publisher] --1--> merge_planner
```
