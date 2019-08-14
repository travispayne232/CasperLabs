package io.casperlabs.casper

import cats.effect._
import cats.effect.concurrent._
import cats.implicits._
import com.google.protobuf.ByteString
import io.casperlabs.casper.MultiParentCasperRef.MultiParentCasperRef
import io.casperlabs.casper.api.BlockAPI
import io.casperlabs.casper.deploybuffer.DeployStorageReader
import io.casperlabs.metrics.Metrics
import io.casperlabs.shared.{Log, Time}

import scala.concurrent.duration._
import scala.util.control.NonFatal

/** Propose a block automatically whenever a timespan has elapsed or
  * we have more than a certain number of new deploys in the buffer. */
class AutoProposer[F[_]: Bracket[?[_], Throwable]: Time: Log: Metrics: MultiParentCasperRef: DeployStorageReader](
    checkInterval: FiniteDuration,
    maxInterval: FiniteDuration,
    maxCount: Int,
    blockApiLock: Semaphore[F]
) {

  private def run(): F[Unit] = {
    val maxElapsedMillis = maxInterval.toMillis

    def loop(
        // Deploys we tried to propose last time.
        prevDeploys: Set[ByteString],
        // Time we saw the first new deploys after an auto-proposal.
        startMillis: Long
    ): F[Unit] = {

      val snapshot = for {
        currentMillis <- Time[F].currentMillis
        deploys       <- DeployStorageReader[F].readPendingHashes.map(_.toSet)
      } yield (currentMillis, currentMillis - startMillis, deploys)

      snapshot flatMap {
        // Reset time when we see a new deploy.
        case (currentMillis, _, deploys) if deploys.nonEmpty && startMillis == 0 =>
          Time[F].sleep(checkInterval) *> loop(prevDeploys, currentMillis)

        case (_, elapsedMillis, deploys)
            if deploys.nonEmpty
              && deploys != prevDeploys
              && (elapsedMillis >= maxElapsedMillis || deploys.size >= maxCount) =>
          Log[F].info(
            s"Proposing block after ${elapsedMillis} ms with ${deploys.size} pending deploys."
          ) *>
            tryPropose() *>
            loop(deploys, 0)

        case _ =>
          Time[F].sleep(checkInterval) *> loop(prevDeploys, startMillis)
      }
    }

    loop(Set.empty, 0) onError {
      case NonFatal(ex) =>
        Log[F].error(s"Auto-proposal stopped unexpectedly.", ex)
    }
  }

  private def tryPropose(): F[Unit] =
    BlockAPI.propose(blockApiLock).flatMap { blockHash =>
      Log[F].info(s"Proposed block ${PrettyPrinter.buildString(blockHash)}")
    } handleErrorWith {
      case NonFatal(ex) =>
        Log[F].error(s"Could not propose block.", ex)
    }
}

object AutoProposer {

  /** Start the proposal loop in the background. */
  def apply[F[_]: Concurrent: Time: Log: Metrics: MultiParentCasperRef: DeployStorageReader](
      checkInterval: FiniteDuration,
      maxInterval: FiniteDuration,
      maxCount: Int,
      blockApiLock: Semaphore[F]
  ): Resource[F, AutoProposer[F]] =
    Resource[F, AutoProposer[F]] {
      for {
        ap    <- Sync[F].delay(new AutoProposer(checkInterval, maxInterval, maxCount, blockApiLock))
        fiber <- Concurrent[F].start(ap.run())
      } yield ap -> fiber.cancel
    }
}
